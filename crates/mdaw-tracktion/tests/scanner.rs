//! Plugins examined by the plugin scanner, in child processes: a plugin that crashes and one
//! that hangs while they are looked at are listed as failed, and the application lives on; the
//! good plugin is offered, loads and plays. A test binary of its own, because it sets CLAP_PATH
//! and the engine is one per process. Files go to `target/bridge-scratch/`.
//!
//! The crashing and hanging plugins (`native/tracktion-bridge/test/bad_clap.c`) are built next to
//! the scanner by `mdaw-tracktion-sys`. The test synth is played if it was built (see
//! `tests/engine.rs`).

use mdaw_model::{
    ActorId, Clip, Device, DeviceKind, IdGen, Note, Op, PluginFormat, Project, TempoPoint, Tick,
    TrackContent, TrackShell,
};
use mdaw_tracktion::{Config, Engine, Problem, built_scanner_path, load_project};
use std::path::{Path, PathBuf};
use std::time::Instant;

const Q: i64 = 960;
const TEST_SYNTH: &str = "org.mdaw.test-synth";

fn scratch() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/bridge-scratch")
}

fn fresh_dir(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
    std::fs::create_dir_all(path).unwrap();
}

/// The peak of a 24-bit wav file's samples.
fn peak(path: &Path) -> f64 {
    let data = std::fs::read(path).expect("render file");
    let start = data.windows(4).position(|w| w == b"data").expect("a data chunk") + 8;
    data[start..]
        .chunks_exact(3)
        .map(|s| f64::from(i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8).abs() / 8_388_608.0)
        .fold(0.0, f64::max)
}

#[test]
fn bad_plugins_are_examined_in_a_child_process() {
    let scanner = built_scanner_path().expect("the build made the scanner");
    let built = scanner.parent().expect("the scanner's folder");
    let clap = scratch().join("rust-scan-clap");
    let settings = scratch().join("rust-scan-settings");
    fresh_dir(&clap);
    fresh_dir(&settings);
    for name in ["bad-crash.clap", "bad-hang.clap"] {
        std::fs::copy(built.join(name), clap.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    let synth = scratch().join("clap/mdaw-test-synth.clap");
    let have_synth = synth.is_file() && std::fs::copy(&synth, clap.join("mdaw-test-synth.clap")).is_ok();
    // SAFETY: set before the engine (and any other thread of this test binary that could read
    // the environment) starts; this binary has a single test.
    unsafe {
        std::env::set_var("CLAP_PATH", &clap);
        std::env::set_var("MDAW_SCAN_TIMEOUT_MS", "3000");
    }

    let mut config = Config::new("mdaw-tracktion-scanner-test", &settings);
    config.scanner_path = Some(scanner.to_path_buf());
    let engine = Engine::new(config).expect("create the engine with the scanner");

    let started = Instant::now();
    engine.plugins_scan("CLAP").expect("scan CLAP with the scanner");
    println!("scan: {:?}", started.elapsed());
    let failures = engine.plugins_failed().expect("failed list");
    println!("failed: {failures:#?}");
    let reason = |name: &str| {
        failures
            .iter()
            .find(|f| f.format == "CLAP" && f.file.ends_with(name))
            .map(|f| f.reason.clone())
            .unwrap_or_default()
    };
    assert!(
        reason("bad-crash.clap").contains("crashed"),
        "the crashing plugin is listed as crashed"
    );
    assert!(
        reason("bad-hang.clap").contains("hung"),
        "the hanging plugin is listed as hung"
    );
    let plugins = engine.plugins().expect("plugin list");
    assert!(
        plugins
            .iter()
            .all(|p| !p.identifier.starts_with("org.mdaw.test-bad")),
        "bad plugins are not offered: {plugins:?}"
    );

    // Nothing changed: the bad files are not examined again (that would take the time limit).
    let started = Instant::now();
    engine.plugins_scan("CLAP").expect("scan again");
    assert!(
        started.elapsed().as_secs_f64() < 1.0,
        "second scan: {:?}",
        started.elapsed()
    );
    assert_eq!(
        engine.plugins_failed().expect("failed list").len(),
        failures.len()
    );

    if !have_synth {
        println!(
            "NOTE: target/bridge-scratch/clap/mdaw-test-synth.clap is missing; the CLAP synth is not played"
        );
        return;
    }
    assert!(
        plugins
            .iter()
            .any(|p| p.format == "CLAP" && p.identifier == TEST_SYNTH),
        "the test synth is offered under its CLAP id: {plugins:?}"
    );

    // A model with the test synth, loaded and rendered.
    let mut ids = IdGen::new(ActorId(3));
    let (tempo, track, device, clip) = (ids.tempo(), ids.track(), ids.device(), ids.clip());
    let mut content = TrackContent::default();
    content.devices.push_back(Device::new(
        device,
        DeviceKind::Plugin {
            format: PluginFormat::Clap,
            id: TEST_SYNTH.into(),
            state: None,
        },
    ));
    let mut notes = Clip {
        start: Tick(0),
        length: Tick(4 * Q),
        name: "Scanned".into(),
        notes: Default::default(),
    };
    for beat in 0..4 {
        notes.notes.insert(
            ids.note(),
            Note {
                start: Tick(beat * Q),
                dur: Tick(Q / 2),
                pitch: 60,
                vel: 100,
            },
        );
    }
    content.clips.insert(clip, notes.into());
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
                name: "Scanned".into(),
                color: 0,
            },
            content,
            after: None,
        },
    ] {
        assert!(mdaw_model::apply(&mut project, &op).is_some(), "{op:?}");
    }
    let problems = load_project(&engine, &project, &|_| None).expect("load");
    assert!(
        !problems
            .iter()
            .any(|p| matches!(p, Problem::DeviceSkipped { .. })),
        "the scanned synth loads: {problems:?}"
    );
    let wav = scratch().join("rust-scanned-synth.wav");
    let _ = std::fs::remove_file(&wav);
    engine.render(&wav, 0, 8 * Q, 44_100.0).expect("render");
    let peak = peak(&wav);
    println!("scanned synth peak: {peak:.4}");
    assert!(peak > 0.01, "the scanned synth plays ({peak})");
}
