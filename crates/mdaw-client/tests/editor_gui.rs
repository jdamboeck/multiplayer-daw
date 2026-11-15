//! Plugin windows through `TracktionAudio`, with a display: the window states the device tab
//! shows (`Opening`, `Open`, `Closing`, `Closed`) follow the engine's answers, quick toggles end
//! where the last click asked, a device removed takes its window along, a plugin whose window
//! fails or is slow to come, and a window opened while the scan waits for a hanging plugin.
//!
//! Opens windows, so it is ignored and also returns at once unless `MDAW_TEST_GUI=1`. Run it on
//! a virtual display only, through the script that prepares one for JUCE:
//!
//! ```sh
//! native/tracktion-bridge/test/run_gui_tests.sh \
//!     cargo test -p mdaw-client --test editor_gui -- --ignored --nocapture
//! ```
//!
//! The plugins are `test-gui.clap` and `bad-hang.clap` (native/tracktion-bridge/test/bad_clap.c),
//! built next to the plugin scanner. Scratch files go to `target/phase5-scratch/editor-gui/`.

use mdaw_client::Audio;
use mdaw_client::audio::{DeviceStatus, EditorState};
use mdaw_client::audio_tracktion::{TracktionAudio, TracktionOptions};
use mdaw_model::{
    ActorId, Device, DeviceId, DeviceKind, IdGen, Op, PluginFormat, Project, TempoPoint, Tick, TrackContent,
    TrackId, TrackShell,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const GUI_PLUGIN: &str = "org.mdaw.test-gui";
const TIMEOUT: Duration = Duration::from_secs(60);

fn scratch() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/phase5-scratch/editor-gui")
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

fn add_gui_track(project: &mut Project, track: TrackId, device: DeviceId) {
    let mut content = TrackContent::default();
    content.devices.push_back(Device::new(
        device,
        DeviceKind::Plugin {
            format: PluginFormat::Clap,
            id: GUI_PLUGIN.into(),
            name: "Test GUI".into(),
            state: None,
        },
    ));
    apply(
        project,
        Op::TrackAdd {
            id: track,
            shell: TrackShell {
                name: "GUI".into(),
                color: 0,
            },
            content,
            after: None,
        },
    );
}

fn set_env(name: &str, value: &str) {
    // SAFETY: every variable changed here was set before the first engine started, so only its
    // value is replaced; the plugin reads them with getenv at each call. One test per binary.
    unsafe { std::env::set_var(name, value) };
}

/// Frames until `done`; the notices of those frames.
fn until(
    audio: &mut TracktionAudio,
    project: &Project,
    what: &str,
    mut done: impl FnMut(&mut TracktionAudio) -> bool,
) -> Vec<String> {
    let started = Instant::now();
    let mut notices = Vec::new();
    while !done(audio) {
        assert!(started.elapsed() < TIMEOUT, "{what}: not after {TIMEOUT:?}");
        notices.extend(audio.frame(project));
        std::thread::sleep(Duration::from_millis(10));
    }
    notices
}

/// Frames until the window of `device` is no longer on its way, checking it never shows `not`
/// meanwhile; where it ended.
fn settle(
    audio: &mut TracktionAudio,
    project: &Project,
    track: TrackId,
    device: DeviceId,
    not: EditorState,
) -> EditorState {
    let mut state = audio.editor_state(track, device);
    until(audio, project, "the window request is answered", |a| {
        state = a.editor_state(track, device);
        assert_ne!(state, not, "a stale answer showed");
        !matches!(state, EditorState::Opening | EditorState::Closing)
    });
    state
}

#[test]
#[ignore = "needs a display; run under xvfb-run with MDAW_TEST_GUI=1"]
fn plugin_windows() {
    if std::env::var("MDAW_TEST_GUI").as_deref() != Ok("1") {
        eprintln!("MDAW_TEST_GUI=1 not set: skipped");
        return;
    }
    let scanner = mdaw_tracktion::built_scanner_path().expect("the build made the scanner");
    let built = scanner.parent().expect("the scanner's folder");
    let clap = scratch().join("clap");
    let settings = scratch().join("settings");
    fresh_dir(&clap);
    fresh_dir(&settings);
    std::fs::copy(built.join("test-gui.clap"), clap.join("test-gui.clap"))
        .expect("test-gui.clap is built next to the scanner");
    // SAFETY: before the first engine and any other thread of this binary (it has one test).
    unsafe {
        std::env::set_var("CLAP_PATH", &clap);
        std::env::set_var("MDAW_SCAN_TIMEOUT_MS", "6000");
        std::env::set_var("MDAW_TEST_GUI_DELAY_MS", "0");
        std::env::set_var("MDAW_TEST_GUI_FAIL", "");
    }
    let mut ids = IdGen::new(ActorId(9));
    let mut project = Project::default();
    apply(
        &mut project,
        Op::TempoPut {
            id: ids.tempo(),
            point: TempoPoint {
                tick: Tick(0),
                milli_bpm: 120_000,
            },
        },
    );
    let (t1, g1, t2, g2) = (ids.track(), ids.device(), ids.track(), ids.device());
    add_gui_track(&mut project, t1, g1);
    add_gui_track(&mut project, t2, g2);

    let mut audio = TracktionAudio::new(options(&settings)).expect("the engine starts");
    until(&mut audio, &project, "the scan ends", |a| !a.scanning_plugins());
    audio.set_project(&project);
    until(&mut audio, &project, "both devices run", |a| {
        a.device_status(&project, t1, g1) == DeviceStatus::Running
            && a.device_status(&project, t2, g2) == DeviceStatus::Running
    });

    // Opening, then open.
    audio.toggle_editor(t1, g1, "GUI").unwrap();
    assert_eq!(audio.editor_state(t1, g1), EditorState::Opening);
    assert!(!audio.editor_open(t1, g1));
    assert_eq!(
        settle(&mut audio, &project, t1, g1, EditorState::Closed),
        EditorState::Open
    );
    assert!(audio.editor_open(t1, g1));

    // Two quick clicks: close and open again; it ends open, never closed on the way.
    audio.toggle_editor(t1, g1, "GUI").unwrap();
    assert_eq!(audio.editor_state(t1, g1), EditorState::Closing);
    audio.toggle_editor(t1, g1, "GUI").unwrap();
    assert_eq!(audio.editor_state(t1, g1), EditorState::Opening);
    assert_eq!(
        settle(&mut audio, &project, t1, g1, EditorState::Closed),
        EditorState::Open
    );
    // Three: close, open, close; it ends closed, never open on the way.
    for _ in 0..3 {
        audio.toggle_editor(t1, g1, "GUI").unwrap();
    }
    assert_eq!(audio.editor_state(t1, g1), EditorState::Closing);
    assert_eq!(
        settle(&mut audio, &project, t1, g1, EditorState::Open),
        EditorState::Closed
    );

    // Two windows at once.
    audio.toggle_editor(t1, g1, "GUI").unwrap();
    audio.toggle_editor(t2, g2, "GUI 2").unwrap();
    assert_eq!(
        settle(&mut audio, &project, t1, g1, EditorState::Closed),
        EditorState::Open
    );
    assert_eq!(
        settle(&mut audio, &project, t2, g2, EditorState::Closed),
        EditorState::Open
    );
    assert!(audio.editor_open(t1, g1) && audio.editor_open(t2, g2));

    // A device removed while its window is open: closed, and the other stays open.
    apply(&mut project, Op::DeviceRemove { track: t2, id: g2 });
    audio.set_project(&project);
    audio.frame(&project);
    assert_eq!(audio.editor_state(t2, g2), EditorState::Closed);
    audio.wait_idle();
    for _ in 0..5 {
        audio.frame(&project);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(audio.editor_state(t2, g2), EditorState::Closed);
    assert!(audio.editor_open(t1, g1), "the other window is still open");
    audio.toggle_editor(t1, g1, "GUI").unwrap();
    assert_eq!(
        settle(&mut audio, &project, t1, g1, EditorState::Open),
        EditorState::Closed
    );

    // A plugin that fails to make its window: closed, and the user is told.
    set_env("MDAW_TEST_GUI_FAIL", "1");
    audio.toggle_editor(t1, g1, "GUI").unwrap();
    let mut notices = Vec::new();
    until(&mut audio, &project, "the failed window is answered", |a| {
        notices.extend(a.frame(&project));
        let state = a.editor_state(t1, g1);
        assert_ne!(state, EditorState::Open, "a failed window never shows open");
        state == EditorState::Closed
    });
    println!("RESULT notices: {notices:?}");
    assert!(
        notices.iter().any(|n| n.contains("window did not open")),
        "{notices:?}"
    );
    set_env("MDAW_TEST_GUI_FAIL", "");

    // A plugin slow to make its window: Opening meanwhile, then open.
    set_env("MDAW_TEST_GUI_DELAY_MS", "500");
    let asked = Instant::now();
    audio.toggle_editor(t1, g1, "GUI").unwrap();
    std::thread::sleep(Duration::from_millis(200));
    audio.frame(&project);
    assert_eq!(audio.editor_state(t1, g1), EditorState::Opening);
    assert_eq!(
        settle(&mut audio, &project, t1, g1, EditorState::Closed),
        EditorState::Open
    );
    println!("RESULT slow window open after {:?}", asked.elapsed());
    set_env("MDAW_TEST_GUI_DELAY_MS", "0");
    audio.toggle_editor(t1, g1, "GUI").unwrap();
    assert_eq!(
        settle(&mut audio, &project, t1, g1, EditorState::Open),
        EditorState::Closed
    );
    drop(audio);

    // A window opened while the scan waits for a plugin that hangs opens long before it ends.
    let hang = clap.join("bad-hang.clap");
    std::fs::copy(built.join("bad-hang.clap"), &hang).expect("bad-hang.clap is built next to the scanner");
    std::fs::File::options()
        .write(true)
        .open(&hang)
        .and_then(|f| f.set_modified(SystemTime::now()))
        .expect("touch bad-hang.clap");
    let mut audio = TracktionAudio::new(options(&settings)).expect("the engine starts again");
    audio.set_project(&project);
    until(&mut audio, &project, "the device runs", |a| {
        a.device_status(&project, t1, g1) == DeviceStatus::Running
    });
    assert!(
        audio.scanning_plugins(),
        "the scan still waits for the hanging plugin"
    );
    let asked = Instant::now();
    audio.toggle_editor(t1, g1, "GUI").unwrap();
    assert_eq!(
        settle(&mut audio, &project, t1, g1, EditorState::Closed),
        EditorState::Open
    );
    let took = asked.elapsed();
    println!("RESULT window open during the scan after {took:?}");
    assert!(audio.scanning_plugins(), "opened before the scan ended");
    assert!(took < Duration::from_secs(3), "{took:?}");
    let t = Instant::now();
    drop(audio);
    println!("RESULT drop during the scan: {:?}", t.elapsed());
    let _ = std::fs::remove_file(&hang);
}
