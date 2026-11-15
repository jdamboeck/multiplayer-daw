//! A plugin scan with the scanner blocks only its caller: while the scanner examines a plugin
//! that hangs, the engine loads a project with a plugin it already knows, answers other calls
//! and keeps its position fresh. Cancelling a scan kills the scanner at once. Reading a plugin's
//! state asks the plugin once, also when the state does not fit the first buffer. A test binary
//! of its own (one engine per process, CLAP_PATH); files go to `target/bridge-scratch/`.

use mdaw_model::{
    ActorId, Device, DeviceKind, IdGen, Op, PluginFormat, Project, TempoPoint, Tick, TrackContent, TrackShell,
};
use mdaw_tracktion::{Config, Engine, EntityKind, Problem, Status, built_scanner_path, load_project};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

fn scratch() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/bridge-scratch")
}

fn fresh_dir(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
    std::fs::create_dir_all(path).unwrap();
}

/// Makes the file look changed, so that the scan examines it again.
fn touch(path: &Path, ahead: u64) {
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() + Duration::from_secs(ahead))
        .unwrap();
}

fn new_engine(settings: &Path, scanner: &Path) -> Engine {
    let mut config = Config::new("mdaw-tracktion-scan-concurrency", settings);
    config.scanner_path = Some(scanner.to_path_buf());
    Engine::new(config).expect("create the engine with the scanner")
}

/// A project with one track holding one CLAP device.
fn project_with(plugin: &str) -> (Project, u64) {
    let mut ids = IdGen::new(ActorId(5));
    let (tempo, track, device) = (ids.tempo(), ids.track(), ids.device());
    let mut content = TrackContent::default();
    content.devices.push_back(Device::new(
        device,
        DeviceKind::Plugin {
            format: PluginFormat::Clap,
            id: plugin.into(),
            name: "Plugin".into(),
            state: None,
        },
    ));
    let mut project = Project::default();
    for op in [
        Op::TempoPut {
            id: tempo,
            point: TempoPoint {
                tick: Tick(0),
                milli_bpm: 120_000,
            },
        },
        Op::TrackAdd {
            id: track,
            shell: TrackShell {
                name: "Track".into(),
                color: 0,
            },
            content,
            after: None,
        },
    ] {
        assert!(mdaw_model::apply(&mut project, &op).is_some(), "{op:?}");
    }
    (project, device.0.0)
}

#[test]
fn a_scan_blocks_only_its_caller() {
    let scanner = built_scanner_path().expect("the build made the scanner");
    let built = scanner.parent().expect("the scanner's folder");
    let clap = scratch().join("rust-concurrency-clap");
    let settings = scratch().join("rust-concurrency-settings");
    fresh_dir(&clap);
    fresh_dir(&settings);
    for name in ["bad-hang.clap", "test-stateless.clap", "test-gui.clap"] {
        std::fs::copy(built.join(name), clap.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    let hang = clap.join("bad-hang.clap");
    // SAFETY: set before the engine (and any other thread of this test binary that could read
    // the environment) starts; this binary has a single test.
    unsafe {
        std::env::set_var("CLAP_PATH", &clap);
        std::env::set_var("MDAW_SCAN_TIMEOUT_MS", "3000");
    }

    let engine = Arc::new(new_engine(&settings, scanner));
    engine.plugins_scan("CLAP").expect("first scan");
    let plugins = engine.plugins().expect("plugin list");
    assert!(
        plugins.iter().any(|p| p.identifier == "org.mdaw.test-stateless"),
        "the stateless plugin is known: {plugins:?}"
    );

    // One state read asks the plugin once (the plugin sleeps the delay in each save). The test
    // plugin's state fits the wrapper's first buffer; the bridge's C test covers the second
    // call of a state that did not fit.
    {
        let (project, device) = project_with("org.mdaw.test-gui");
        let problems = load_project(&engine, &project, &|_| None).expect("load the gui plugin");
        assert!(
            !problems
                .iter()
                .any(|p| matches!(p, Problem::DeviceSkipped { .. })),
            "the gui plugin loads: {problems:?}"
        );
        // SAFETY: the plugin reads it on this thread, inside the call below; no other thread
        // of this binary reads the environment meanwhile.
        unsafe { std::env::set_var("MDAW_TEST_STATE_DELAY_MS", "300") };
        let started = Instant::now();
        let state = engine.device_get_state(device);
        let took = started.elapsed();
        // SAFETY: as above.
        unsafe { std::env::remove_var("MDAW_TEST_STATE_DELAY_MS") };
        println!("state read: {} bytes in {took:?}", state.len());
        assert!(!state.is_empty());
        assert!(
            took >= Duration::from_millis(300) && took < Duration::from_millis(550),
            "one save per read: {took:?}"
        );
    }

    // A scan that takes the time limit, on a thread of its own.
    touch(&hang, 20);
    let scan = {
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || {
            let started = Instant::now();
            let result = engine.plugins_scan("CLAP");
            (result, started.elapsed())
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    let (project, device) = project_with("org.mdaw.test-stateless");
    let started = Instant::now();
    let problems = load_project(&engine, &project, &|_| None).expect("load during the scan");
    let load_took = started.elapsed();
    assert!(
        !problems
            .iter()
            .any(|p| matches!(p, Problem::DeviceSkipped { .. })),
        "the known plugin loads during the scan: {problems:?}"
    );
    assert!(engine.has(EntityKind::Device, device));
    let (mut worst_has, mut worst_age, mut samples) = (Duration::ZERO, 0.0f64, 0);
    while !scan.is_finished() {
        let t = Instant::now();
        engine.has(EntityKind::Device, device);
        worst_has = worst_has.max(t.elapsed());
        worst_age = worst_age.max(engine.state().position_age_seconds);
        samples += 1;
        std::thread::sleep(Duration::from_millis(50));
    }
    let (result, scan_took) = scan.join().unwrap();
    println!(
        "scan {scan_took:?}; during it: load {load_took:?}, worst has() {worst_has:?}, worst position age {worst_age:.3} s, {samples} samples"
    );
    result.expect("the scan with the hanging plugin");
    assert!(
        scan_took >= Duration::from_secs(3),
        "the scan waited for the hanging plugin"
    );
    assert!(samples > 10, "sampled while the scan ran");
    assert!(
        load_took < Duration::from_secs(1),
        "load during the scan: {load_took:?}"
    );
    assert!(
        worst_has < Duration::from_secs(1),
        "has() during the scan: {worst_has:?}"
    );
    assert!(
        worst_age < 1.0,
        "the engine's timer ran during the scan: {worst_age}"
    );
    drop(engine);

    // Cancel, on an engine of its own (it is for the engine's life).
    let engine = Arc::new(new_engine(&settings, scanner));
    touch(&hang, 40);
    let scan = {
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || engine.plugins_scan("CLAP"))
    };
    std::thread::sleep(Duration::from_millis(300));
    let cancelled_at = Instant::now();
    engine.plugins_scan_cancel().expect("cancel");
    let result = scan.join().unwrap();
    let took = cancelled_at.elapsed();
    println!(
        "cancelled scan returned {:?} after {took:?}",
        result.as_ref().map_err(|e| e.status)
    );
    assert_eq!(result.map_err(|e| e.status), Err(Status::Cancelled));
    assert!(
        took < Duration::from_secs(3),
        "the cancelled scan returned in {took:?}"
    );
    assert_eq!(
        engine.plugins_scan("CLAP").map_err(|e| e.status),
        Err(Status::Cancelled)
    );
}
