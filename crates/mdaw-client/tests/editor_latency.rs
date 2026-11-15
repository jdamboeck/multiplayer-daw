//! Benchmark of the client's engine worker as it is used behind a plugin's window, through the
//! public API of `TracktionAudio` alone, so it can be run again after the worker changes:
//!
//! - A: a plugin whose state takes 800 ms to read, looked at as if its window were open while
//!   the UI runs frames for 8 s: how long the worker then takes to catch up (`wait_idle`).
//! - B: a project handed over while the startup scan examines a plugin that hangs until the
//!   time limit: how long until the device is in the engine, and until the scan is over.
//!
//! Not a check: it prints `bench <name> <value> <unit>` lines. Run by hand (one engine per
//! process, so this binary has one test):
//!
//! ```sh
//! cargo test -p mdaw-client --features tracktion --test editor_latency -- --ignored --nocapture
//! ```
//!
//! The plugin is `test-gui.clap` (native/tracktion-bridge/test/bad_clap.c), built next to the
//! plugin scanner, as is `bad-hang.clap`. Scratch files go to `target/phase5-scratch/editor-latency/`.

use mdaw_client::Audio;
use mdaw_client::audio::DeviceStatus;
use mdaw_client::audio_tracktion::{TracktionAudio, TracktionOptions};
use mdaw_model::{
    ActorId, Device, DeviceId, DeviceKind, IdGen, Op, PluginFormat, Project, TempoPoint, Tick, TrackContent,
    TrackId, TrackShell,
};
use mdaw_tracktion::EntityKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const GUI_PLUGIN: &str = "org.mdaw.test-gui";
const STATE_DELAY_MS: &str = "800";
const TIMEOUT: Duration = Duration::from_secs(60);

fn scratch() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/phase5-scratch/editor-latency")
}

fn fresh_dir(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
    std::fs::create_dir_all(path).unwrap();
}

fn bench(name: &str, value: Duration) {
    println!("bench {name} {:.1} ms", value.as_secs_f64() * 1000.0);
}

/// One track with the test-gui plugin on it.
fn project() -> (Project, TrackId, DeviceId) {
    let mut ids = IdGen::new(ActorId(7));
    let (tempo, track, device) = (ids.tempo(), ids.track(), ids.device());
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
                name: "Editor latency".into(),
                color: 0,
            },
            content,
            after: None,
        },
    ] {
        assert!(mdaw_model::apply(&mut project, &op).is_some(), "{op:?}");
    }
    (project, track, device)
}

fn options(settings: &Path) -> TracktionOptions {
    let mut options = TracktionOptions::new(settings);
    options.scan = vec![PluginFormat::Clap];
    options
}

#[test]
#[ignore = "benchmark; run by hand"]
fn editor_latency() {
    let scanner = mdaw_tracktion::built_scanner_path().expect("the build made the scanner");
    let built = scanner.parent().expect("the scanner's folder");
    let clap = scratch().join("clap");
    let settings = scratch().join("settings");
    fresh_dir(&clap);
    fresh_dir(&settings);
    std::fs::copy(built.join("test-gui.clap"), clap.join("test-gui.clap"))
        .expect("test-gui.clap is built next to the scanner");
    // SAFETY: set before the first engine (and any other thread of this test binary that could
    // read the environment) starts; this binary has a single test. The delay is set to "0" here
    // so that changing it later, while the engine runs, only replaces the value of a variable
    // that exists (the plugin reads it with getenv on the engine's thread).
    unsafe {
        std::env::set_var("CLAP_PATH", &clap);
        std::env::set_var("MDAW_SCAN_TIMEOUT_MS", "5000");
        std::env::set_var("MDAW_TEST_STATE_DELAY_MS", "0");
    }
    let (project, track, device) = project();
    let raw_device = device.0.0;

    // ---- A: the worker's backlog behind slow state reads
    let mut audio = TracktionAudio::new(options(&settings)).expect("the engine starts");
    let started = Instant::now();
    audio.set_project(&project);
    loop {
        audio.frame(&project);
        let status = audio.device_status(&project, track, device);
        if status == DeviceStatus::Running {
            break;
        }
        assert!(
            started.elapsed() < TIMEOUT,
            "the test-gui device did not start: {status:?}"
        );
        std::thread::sleep(Duration::from_millis(16));
    }
    bench("device_running_ms", started.elapsed());

    let t = Instant::now();
    audio.wait_idle();
    bench("idle_wait_idle_ms", t.elapsed());

    audio.watch(device);
    // SAFETY: the variable exists (set above), so only its value is replaced; see above.
    unsafe { std::env::set_var("MDAW_TEST_STATE_DELAY_MS", STATE_DELAY_MS) };
    let t = Instant::now();
    let mut frames = 0u32;
    while t.elapsed() < Duration::from_secs(8) {
        audio.frame(&project);
        frames += 1;
        std::thread::sleep(Duration::from_millis(16));
    }
    println!(
        "# {frames} frames in {:?} with state reads of {STATE_DELAY_MS} ms",
        t.elapsed()
    );
    let t = Instant::now();
    audio.wait_idle();
    bench("backlog_wait_idle_ms", t.elapsed());
    println!("# queue after A: {:?}", audio.queue_stats());
    // SAFETY: as above.
    unsafe { std::env::set_var("MDAW_TEST_STATE_DELAY_MS", "0") };
    drop(audio);

    // ---- B: a project handed over while the scan waits for a plugin that hangs
    let hang = clap.join("bad-hang.clap");
    std::fs::copy(built.join("bad-hang.clap"), &hang).expect("bad-hang.clap is built next to the scanner");
    std::fs::File::options()
        .write(true)
        .open(&hang)
        .and_then(|f| f.set_modified(SystemTime::now()))
        .expect("touch bad-hang.clap");

    let created = Instant::now();
    let mut audio = TracktionAudio::new(options(&settings)).expect("the engine starts again");
    bench("engine_create_ms", created.elapsed());
    let given = Instant::now();
    audio.set_project(&project);
    let engine = audio.engine();
    let ready = std::thread::scope(|s| {
        let ready = s.spawn(|| {
            // `has` waits for the engine's thread like every other call.
            while given.elapsed() < TIMEOUT {
                if engine.has(EntityKind::Device, raw_device) {
                    return Some(given.elapsed());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            None
        });
        let t = Instant::now();
        audio.wait_idle();
        bench("wait_idle_during_scan_ms", t.elapsed());
        ready.join().unwrap()
    });
    match ready {
        Some(ready) => bench("device_ready_during_scan_ms", ready),
        None => println!("# the device was not in the engine after {TIMEOUT:?}"),
    }
    while audio.scanning_plugins() {
        assert!(created.elapsed() < TIMEOUT, "the scan did not end");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Seen only after wait_idle returned: at most this long.
    bench("scan_with_hang_ms", created.elapsed());
    println!("# queue after B: {:?}", audio.queue_stats());
    let failed = audio.engine().plugins_failed().unwrap_or_default();
    println!("# failed plugins: {failed:?}");
    drop(audio);
    let _ = std::fs::remove_file(&hang);
}
