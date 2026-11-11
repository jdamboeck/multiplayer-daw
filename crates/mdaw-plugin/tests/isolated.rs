//! Plugins in helper processes: they sound as they do inside, and when one crashes, the host
//! carries on.
#![cfg(target_os = "linux")]

use mdaw_dsp::{BlockTime, ExternalDevice};
use mdaw_plugin::{Host, Instance, PluginFormat, Settings, TEST_SYNTH};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;
const BLOCK: usize = 256;

fn host() -> Host {
    Host::isolated_test_synth(PathBuf::from(env!("CARGO_BIN_EXE_mdaw-plugin-host")))
}

fn synth(host: &mut Host, offline: bool) -> Box<dyn Instance> {
    let settings = Settings {
        sample_rate: RATE,
        max_block: BLOCK,
        offline,
    };
    host.create(PluginFormat::Clap, TEST_SYNTH, settings).unwrap()
}

fn block(device: &mut dyn ExternalDevice) -> (Vec<f32>, Vec<f32>) {
    let (mut left, mut right) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
    device.process(&mut left, &mut right, &BlockTime::default());
    (left, right)
}

/// A chord for a quarter of a second, then its release.
fn play(device: &mut dyn ExternalDevice) -> (Vec<f32>, Vec<f32>) {
    device.note_on(0, 1, 60, 100);
    device.note_on(0, 2, 64, 100);
    let (mut l, mut r) = (Vec::new(), Vec::new());
    for i in 0..60 {
        if i == 47 {
            device.release_where(&|_| true);
        }
        let (bl, br) = block(device);
        l.extend(bl);
        r.extend(br);
    }
    (l, r)
}

fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0, |m, s| m.max(s.abs()))
}

#[test]
fn a_plugin_in_a_helper_sounds_and_keeps_its_state_as_inside() {
    let mut host = host();
    let info = host
        .find(PluginFormat::Clap, TEST_SYNTH)
        .expect("found by a helper")
        .clone();
    assert!(info.instrument);

    let mut first = synth(&mut host, true);
    assert_eq!(first.param_count(), 3);
    let mut device = first.take_processor().unwrap();
    first.set_param(mdaw_test_plugin::BALANCE, 1.0);
    assert!(first.changed());
    block(device.as_mut());
    let (left, right) = play(device.as_mut());
    assert_eq!(peak(&left), 0.0, "all to the right");
    assert!(peak(&right) > 0.05);
    let state = first.save_state().unwrap();
    let heard = play(device.as_mut());

    // The same state in another helper: the same samples.
    let mut second = synth(&mut host, true);
    second.load_state(&state).unwrap();
    let mut other = second.take_processor().unwrap();
    block(other.as_mut());
    assert_eq!(play(other.as_mut()), heard);
    assert_eq!(second.params(usize::MAX)[1].value, 1.0);
}

#[test]
fn a_plugin_that_crashes_takes_only_its_helper_down() {
    let mut host = host();
    let mut synth = synth(&mut host, false);
    let mut device = synth.take_processor().unwrap();
    // Live: a block late is a block missed, so listen for a while.
    device.note_on(0, 1, 60, 100);
    let deadline = Instant::now() + Duration::from_secs(5);
    while peak(&block(device.as_mut()).0) < 0.05 {
        assert!(Instant::now() < deadline, "it plays");
    }

    // Told to, the test synth crashes on this pitch.
    synth.set_param(mdaw_test_plugin::MISBEHAVE, mdaw_test_plugin::CRASH);
    block(device.as_mut());
    device.note_on(0, 2, mdaw_test_plugin::CRASH_PITCH, 100);
    let started = Instant::now();
    for _ in 0..20 {
        let (left, right) = block(device.as_mut());
        // Its blocks pass through unchanged: silent, here.
        assert!(
            left.iter().chain(&right).all(|s| *s == 0.0) || started.elapsed() < Duration::from_millis(50)
        );
    }
    // Each block waits at most a little longer than itself, crash or not.
    assert!(
        started.elapsed() < Duration::from_millis(20 * 10),
        "{:?}",
        started.elapsed()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !synth.crashed() {
        synth.idle();
        assert!(Instant::now() < deadline, "the crash is noticed");
        std::thread::sleep(Duration::from_millis(10));
    }
    // From then on it costs nothing, and asking it things gives empty answers, not a hang.
    let started = Instant::now();
    for _ in 0..100 {
        block(device.as_mut());
    }
    assert!(started.elapsed() < Duration::from_millis(50));
    assert!(synth.save_state().is_err());
    assert!(synth.params(10).is_empty());

    // The host makes a new one as if nothing happened.
    let mut again = self::synth(&mut host, true);
    let mut device = again.take_processor().unwrap();
    assert!(peak(&play(device.as_mut()).0) > 0.05);
}

#[test]
fn an_offline_render_does_not_wait_for_a_helper_that_died() {
    let mut host = host();
    let mut synth = synth(&mut host, true);
    let mut device = synth.take_processor().unwrap();
    synth.set_param(mdaw_test_plugin::MISBEHAVE, mdaw_test_plugin::CRASH);
    block(device.as_mut());
    device.note_on(0, 1, mdaw_test_plugin::CRASH_PITCH, 100);
    let started = Instant::now();
    for _ in 0..10 {
        block(device.as_mut());
    }
    // Offline, a block may take up to ten seconds; a dead helper is known at once.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(synth.crashed());
}

#[test]
fn a_slow_plugin_is_left_out_of_a_block_rather_than_holding_up_the_callback() {
    let mut host = host();
    let mut synth = synth(&mut host, false);
    let mut device = synth.take_processor().unwrap();
    synth.set_param(mdaw_test_plugin::MISBEHAVE, mdaw_test_plugin::SLOW);
    block(device.as_mut());
    device.note_on(0, 1, mdaw_test_plugin::CRASH_PITCH, 100);
    // Each block now takes the plugin 50 ms; the callback gives it 3.
    for _ in 0..10 {
        let deadline = Instant::now() + Duration::from_millis(3);
        let time = BlockTime {
            deadline: Some(deadline),
            ..BlockTime::default()
        };
        let (mut left, mut right) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
        device.process(&mut left, &mut right, &time);
        let late = Instant::now().saturating_duration_since(deadline);
        assert!(late < Duration::from_millis(2), "{late:?} past the deadline");
    }
}

#[test]
fn a_flood_of_notes_leaves_none_stuck() {
    for isolated in [false, true] {
        let mut host = if isolated { host() } else { Host::test_synth_only() };
        let mut synth = synth(&mut host, true);
        let mut device = synth.take_processor().unwrap();
        // Far more than fit in one block's events: some do not start, but every one that
        // does is ended.
        for key in 0..600u64 {
            device.note_on(0, key, 40 + (key % 40) as u8, 100);
        }
        for key in 0..600u64 {
            device.note_off(0, key);
        }
        for _ in 0..40 {
            block(device.as_mut());
        }
        let (left, _) = block(device.as_mut());
        assert_eq!(peak(&left), 0.0, "isolated: {isolated}: notes are stuck");
    }
}

#[test]
fn a_plugin_that_hangs_in_its_processing_is_ended() {
    let mut host = host();
    let mut synth = synth(&mut host, false);
    let mut device = synth.take_processor().unwrap();
    synth.set_param(mdaw_test_plugin::MISBEHAVE, mdaw_test_plugin::HANG);
    block(device.as_mut());
    device.note_on(0, 1, mdaw_test_plugin::CRASH_PITCH, 100);
    // Its main thread still answers; its audio never comes back.
    let started = Instant::now();
    while !synth.crashed() {
        let time = BlockTime {
            deadline: Some(Instant::now() + Duration::from_millis(3)),
            ..BlockTime::default()
        };
        let (mut left, mut right) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
        device.process(&mut left, &mut right, &time);
        synth.idle();
        std::thread::sleep(Duration::from_millis(5));
        assert!(started.elapsed() < Duration::from_secs(10), "the hang is noticed");
    }
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "not before it has hung a while"
    );
}

#[test]
fn letting_a_helper_go_does_not_wait_for_it() {
    let mut host = host();
    let synth = synth(&mut host, true);
    let started = Instant::now();
    drop(synth);
    assert!(
        started.elapsed() < Duration::from_millis(20),
        "{:?}",
        started.elapsed()
    );
}
