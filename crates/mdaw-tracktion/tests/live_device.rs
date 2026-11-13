//! Live notes and playback with an open audio device. Needs a sound device and makes sound, so it
//! is ignored by default:
//!
//! ```sh
//! cargo test -p mdaw-tracktion --test live_device -- --ignored --nocapture
//! ```
//!
//! One engine per process: one test.

use mdaw_tracktion::{Config, Engine};
use std::path::Path;
use std::time::{Duration, Instant};

/// The highest master peak seen in `for_ms`.
fn peak_over(engine: &Engine, for_ms: u64) -> f32 {
    let until = Instant::now() + Duration::from_millis(for_ms);
    let mut peak = 0.0f32;
    while Instant::now() < until {
        let state = engine.state();
        peak = peak.max(state.peak_left).max(state.peak_right);
        std::thread::sleep(Duration::from_millis(10));
    }
    peak
}

#[test]
#[ignore = "opens the audio device and makes sound"]
fn live_notes_are_heard_while_stopped_and_while_playing() {
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/bridge-scratch");
    let mut config = Config::new("mdaw-live-test", scratch.join("live-settings"));
    config.open_audio_device = true;
    let engine = Engine::new(config).expect("engine");
    let opened = Instant::now() + Duration::from_secs(5);
    while !engine.state().device_open && Instant::now() < opened {
        std::thread::sleep(Duration::from_millis(50));
    }
    let state = engine.state();
    println!(
        "RESULT device_open={} sample_rate={} block={} output_latency_ms={:.2}",
        state.device_open,
        state.sample_rate,
        state.block_size,
        state.output_latency_seconds * 1000.0
    );
    assert!(state.device_open, "no audio device opened");
    assert!(
        state.output_latency_seconds > 0.0,
        "an open device has an output latency"
    );

    // A track with the built-in synth and no clips.
    engine.track_add(1, 0, "live").unwrap();
    engine.device_add(1, 2, 0, "builtin", "4osc").unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let quiet = peak_over(&engine, 300);

    engine.track_live_note(1, 60, 100).unwrap();
    let stopped = peak_over(&engine, 400);
    engine.track_live_note(1, 60, 0).unwrap();
    let released = {
        std::thread::sleep(Duration::from_millis(1000));
        // The peaks are "since the last read": clear what built up during the wait.
        engine.state();
        std::thread::sleep(Duration::from_millis(50));
        engine.state();
        peak_over(&engine, 300)
    };
    println!("RESULT quiet={quiet:.5} live_note_stopped={stopped:.5} released={released:.5}");
    assert!(quiet < 1e-4, "silent before the note");
    assert!(stopped > 0.01, "the note is heard while the transport stands");
    assert!(released < 1e-3, "and ends");

    // While playing: the position advances, and a live note is heard over it.
    engine.transport_play().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let from = engine.state().position_seconds;
    engine.track_live_note(1, 64, 100).unwrap();
    let playing = peak_over(&engine, 400);
    engine.track_live_note(1, 64, 0).unwrap();
    let to = engine.state();
    println!(
        "RESULT live_note_playing={playing:.5} advanced_s={:.3} age_ms={:.1}",
        to.position_seconds - from,
        to.position_age_seconds * 1000.0
    );
    assert!(playing > 0.01, "the note is heard while playing");
    assert!(to.position_seconds - from > 0.2, "the position advances");
    assert!(to.position_age_seconds < 0.2, "the position is fresh");
    engine.transport_stop().unwrap();

    // How soon the note is heard: from the call to the first block with sound in the meter
    // (the meter is read every ~33 ms, so this is an upper bound plus the output latency).
    std::thread::sleep(Duration::from_millis(1500));
    engine.state();
    peak_over(&engine, 100);
    let mut delays = Vec::new();
    for pitch in [55u8, 57, 59, 60, 62] {
        let sent = Instant::now();
        engine.track_live_note(1, pitch, 110).unwrap();
        while Instant::now() - sent < Duration::from_millis(500) {
            let s = engine.state();
            if s.peak_left.max(s.peak_right) > 0.01 {
                delays.push((Instant::now() - sent).as_secs_f64() * 1000.0);
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        engine.track_live_note(1, pitch, 0).unwrap();
        std::thread::sleep(Duration::from_millis(1200));
        peak_over(&engine, 100);
    }
    println!("RESULT live_note_to_meter_ms={delays:?}");
    assert_eq!(delays.len(), 5, "every note reached the meter");
}
