//! The CLAP backend against the built-in test synth: found, made, played, saved and restored.

use mdaw_dsp::{BlockTime, ExternalDevice};
use mdaw_plugin::{Host, Instance, PluginFormat, Settings, TEST_SYNTH};

const RATE: u32 = 48_000;
const BLOCK: usize = 256;

fn settings(offline: bool) -> Settings {
    Settings {
        sample_rate: RATE,
        max_block: BLOCK,
        offline,
    }
}

fn synth(host: &mut Host) -> Box<dyn Instance> {
    host.create(PluginFormat::Clap, TEST_SYNTH, settings(true))
        .unwrap()
}

/// Plays a chord for `frames`, then lets go, and renders the release too.
fn play(device: &mut dyn ExternalDevice, frames: usize) -> (Vec<f32>, Vec<f32>) {
    let (mut all_left, mut all_right) = (Vec::new(), Vec::new());
    for (key, pitch) in [(1, 60), (2, 64)] {
        device.note_on(0, key, pitch, 100);
    }
    let mut done = 0;
    while done < frames + RATE as usize / 10 {
        if done >= frames {
            device.release_where(&|_| true);
        }
        let (mut left, mut right) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
        device.process(&mut left, &mut right, &BlockTime::default());
        all_left.extend(left);
        all_right.extend(right);
        done += BLOCK;
    }
    (all_left, all_right)
}

fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0, |m, s| m.max(s.abs()))
}

#[test]
fn the_test_synth_is_found_and_plays() {
    let mut host = Host::test_synth_only();
    let info = host.find(PluginFormat::Clap, TEST_SYNTH).expect("found").clone();
    assert!(info.instrument);
    assert_eq!(info.name, "mdaw Test Synth");
    assert!(
        host.create(PluginFormat::Clap, "org.example.nothing", settings(true))
            .is_err()
    );

    let mut synth = synth(&mut host);
    let names: Vec<String> = synth.params(usize::MAX).into_iter().map(|p| p.name).collect();
    assert_eq!(names, ["Level", "Balance", "Misbehave on lowest note"]);
    let mut device = synth.take_processor().unwrap();
    assert!(synth.take_processor().is_none(), "there is one processor");
    assert!(synth.busy());
    let (left, right) = play(device.as_mut(), RATE as usize / 4);
    assert!(peak(&left) > 0.05 && peak(&right) > 0.05);
    // The release ends in silence.
    assert_eq!(peak(&left[left.len() - BLOCK..]), 0.0);

    // The processor comes back to its instance once it is let go of.
    device.retire();
    drop(device);
    synth.idle();
    assert!(!synth.busy());
    host.dispose(synth);
    host.idle();
}

#[test]
fn a_parameter_set_from_outside_changes_the_sound_and_marks_the_state_changed() {
    let mut host = Host::test_synth_only();
    let mut synth = synth(&mut host);
    let mut device = synth.take_processor().unwrap();
    assert!(!synth.changed());
    synth.set_param(mdaw_test_plugin::BALANCE, 1.0);
    assert!(synth.changed());
    let (left, right) = play(device.as_mut(), RATE as usize / 4);
    assert_eq!(peak(&left), 0.0, "all to the right");
    assert!(peak(&right) > 0.05);
    let balance = synth
        .params(usize::MAX)
        .into_iter()
        .find(|p| p.name == "Balance")
        .unwrap();
    assert_eq!(balance.value, 1.0);
    device.retire();
}

#[test]
fn a_saved_state_makes_another_instance_sound_the_same() {
    let mut host = Host::test_synth_only();
    let mut first = synth(&mut host);
    let mut device = first.take_processor().unwrap();
    first.set_param(mdaw_test_plugin::LEVEL, 0.2);
    first.set_param(mdaw_test_plugin::BALANCE, -0.5);
    // Parameters reach the plugin with its next block.
    let mut silence = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
    device.process(&mut silence.0, &mut silence.1, &BlockTime::default());
    let state = first.save_state().unwrap();
    assert!(!first.changed(), "saved");
    let heard = play(device.as_mut(), RATE as usize / 4);
    device.retire();

    let mut second = synth(&mut host);
    second.load_state(&state).unwrap();
    assert!(!second.changed());
    let mut other = second.take_processor().unwrap();
    assert_eq!(play(other.as_mut(), RATE as usize / 4), heard, "the same samples");
    other.retire();

    assert!(second.load_state(b"something else").is_err());
}

/// Every instrument installed on this machine: found, made offline, played, and saved. Run by
/// hand with `cargo test -p mdaw-plugin --test host -- --ignored installed --nocapture`.
#[test]
#[ignore = "depends on the plugins installed here"]
fn installed_instruments_load_and_play() {
    let mut host = Host::standard(true);
    for info in host.catalog().to_vec() {
        if !info.instrument {
            continue;
        }
        let mut instance = match host.create(info.format, &info.id, settings(true)) {
            Ok(instance) => instance,
            Err(err) => {
                eprintln!("{:40} could not be made: {err}", info.name);
                continue;
            }
        };
        let params = instance.param_count();
        let state = instance.save_state().map(|s| s.len());
        let mut device = instance.take_processor().unwrap();
        let (left, right) = play(device.as_mut(), RATE as usize / 2);
        device.retire();
        drop(device);
        instance.idle();
        eprintln!(
            "{:40} {params:4} params, state {:?} bytes, peak {:.3} / {:.3}",
            info.name,
            state,
            peak(&left),
            peak(&right)
        );
        host.dispose(instance);
    }
    host.idle();
}

/// Opens and closes the window of an installed plugin three times, then lets the instance go.
/// Run by hand with
/// `MDAW_EDITOR=<plugin id> cargo test -p mdaw-plugin --test host -- --ignored editor --nocapture`.
/// How the window is closed: by the host (default); `MDAW_EDITOR_WM_CLOSE=1` as a window
/// manager asks (`MDAW_EDITOR_UNMAP=1` unmaps it first); `MDAW_EDITOR_NIRI=1` through the niri
/// compositor; `MDAW_EDITOR_KILL=1` by cutting off the window's X11 connection, which takes the
/// process down with most plugins.
#[test]
#[ignore = "needs a display and an installed plugin"]
fn editor_opens_and_closes() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    let Ok(id) = std::env::var("MDAW_EDITOR") else {
        return;
    };
    let mut host = Host::standard(true);
    let mut instance = host.create(PluginFormat::Clap, &id, settings(false)).unwrap();
    let mut device = instance.take_processor().unwrap();
    // The audio part runs on a thread of its own, as in the app.
    let stop = Arc::new(AtomicBool::new(false));
    let audio = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let (mut l, mut r) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
                device.process(&mut l, &mut r, &BlockTime::default());
                std::thread::sleep(Duration::from_millis(5));
            }
            device.retire();
            device
        })
    };
    let pump = |instance: &mut Box<dyn Instance>, ms: u64| {
        let until = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < until {
            instance.idle();
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    for round in 0..3 {
        instance.open_editor("editor test").unwrap();
        pump(&mut instance, 1500);
        eprintln!("round {round}: closing");
        if std::env::var_os("MDAW_EDITOR_NIRI").is_some() {
            // The compositor's own close, as its shortcut does it.
            let out = std::process::Command::new("niri")
                .args(["msg", "-j", "windows"])
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            let at = text
                .find("\"title\":\"editor test\"")
                .expect("the window is listed");
            let id_at = text[..at].rfind("\"id\":").expect("an id");
            let id: String = text[id_at + 5..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            eprintln!("closing niri window {id}");
            std::process::Command::new("niri")
                .args(["msg", "action", "close-window", "--id", &id])
                .status()
                .unwrap();
            pump(&mut instance, 500);
            eprintln!("editor open after the close: {}", instance.editor_open());
        } else if std::env::var_os("MDAW_EDITOR_KILL").is_some() {
            wm_kill("editor test");
            pump(&mut instance, 300);
            assert!(!instance.editor_open(), "gone with its window");
        } else if std::env::var_os("MDAW_EDITOR_WM_CLOSE").is_some() {
            wm_close("editor test");
            pump(&mut instance, 300);
            assert!(!instance.editor_open(), "closed by the window manager");
        }
        instance.close_editor();
        pump(&mut instance, 300);
        eprintln!("round {round}: closed");
    }
    stop.store(true, Ordering::Relaxed);
    drop(audio.join().unwrap());
    instance.idle();
}

/// Asks the window called `title` to close, as a window manager's close button does.
#[cfg(target_os = "linux")]
fn wm_close(title: &str) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::*;
    let (conn, screen) = x11rb::connect(None).unwrap();
    let root = conn.setup().roots[screen].root;
    let atom = |name: &[u8]| conn.intern_atom(false, name).unwrap().reply().unwrap().atom;
    let (protocols, delete) = (atom(b"WM_PROTOCOLS"), atom(b"WM_DELETE_WINDOW"));
    let tree = conn.query_tree(root).unwrap().reply().unwrap();
    for window in tree.children {
        let name = conn
            .get_property(false, window, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 64)
            .unwrap()
            .reply()
            .unwrap();
        if name.value == title.as_bytes() {
            if std::env::var_os("MDAW_EDITOR_UNMAP").is_some() {
                conn.unmap_window(window).unwrap();
            }
            let event = ClientMessageEvent::new(32, window, protocols, [delete, 0, 0, 0, 0]);
            conn.send_event(false, window, EventMask::NO_EVENT, event)
                .unwrap();
            conn.flush().unwrap();
            return;
        }
    }
    panic!("no window called {title}");
}

#[cfg(not(target_os = "linux"))]
fn wm_close(_: &str) {}

/// Cuts off the client that made the window called `title`, as a window manager does with a
/// window that does not close when asked.
#[cfg(target_os = "linux")]
fn wm_kill(title: &str) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::*;
    let (conn, screen) = x11rb::connect(None).unwrap();
    let root = conn.setup().roots[screen].root;
    let tree = conn.query_tree(root).unwrap().reply().unwrap();
    for window in tree.children {
        let name = conn
            .get_property(false, window, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 64)
            .unwrap()
            .reply()
            .unwrap();
        if name.value == title.as_bytes() {
            conn.kill_client(window).unwrap();
            conn.flush().unwrap();
            return;
        }
    }
    panic!("no window called {title}");
}

#[cfg(not(target_os = "linux"))]
fn wm_kill(_: &str) {}

#[test]
fn the_test_synth_misbehaves_only_when_told_and_a_failure_is_reported() {
    let mut host = Host::test_synth_only();
    let mut synth = synth(&mut host);
    let mut device = synth.take_processor().unwrap();
    // By default the lowest note is a note like any other.
    device.note_on(0, 1, mdaw_test_plugin::CRASH_PITCH, 100);
    let mut block = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
    device.process(&mut block.0, &mut block.1, &BlockTime::default());
    assert!(!synth.crashed());

    // Told to fail, its processing fails, and the instance says so: it is not left silent
    // without a word.
    synth.set_param(mdaw_test_plugin::MISBEHAVE, mdaw_test_plugin::FAIL);
    device.process(&mut block.0, &mut block.1, &BlockTime::default());
    device.note_on(0, 2, mdaw_test_plugin::CRASH_PITCH, 100);
    device.process(&mut block.0, &mut block.1, &BlockTime::default());
    assert!(synth.crashed());
    device.retire();
}

/// What the start of an offline render of an installed plugin looks like, as a commit renders
/// it: notes from the very first frame. Run by hand with
/// `MDAW_PROBE=<plugin id> cargo test -p mdaw-plugin --test host -- --ignored probe --nocapture`.
#[test]
#[ignore = "depends on the plugins installed here"]
fn probe_the_start_of_an_offline_render() {
    let Ok(id) = std::env::var("MDAW_PROBE") else {
        return;
    };
    let mut host = Host::standard(false);
    let offline = std::env::var("MDAW_PROBE_LIVE").is_err();
    let mut instance = host.create(PluginFormat::Clap, &id, settings(offline)).unwrap();
    if let Ok(path) = std::env::var("MDAW_PROBE_STATE") {
        instance.load_state(&std::fs::read(path).unwrap()).unwrap();
    }
    let mut device = instance.take_processor().unwrap();
    let warm: usize = std::env::var("MDAW_PROBE_WARM")
        .ok()
        .and_then(|w| w.parse().ok())
        .unwrap_or(0);
    for _ in 0..warm {
        let (mut l, mut r) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
        device.process(
            &mut l,
            &mut r,
            &BlockTime {
                playing: false,
                bpm: 120.0,
                ..BlockTime::default()
            },
        );
    }
    device.note_on(0, 1, 48, 100);
    let mut left = Vec::new();
    for _ in 0..200 {
        let (mut l, mut r) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
        device.process(
            &mut l,
            &mut r,
            &BlockTime {
                playing: true,
                bpm: 120.0,
                ..BlockTime::default()
            },
        );
        left.extend(l);
    }
    let rms =
        |s: &[f32]| (s.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>() / s.len().max(1) as f64).sqrt();
    eprintln!(
        "offline {offline}: rms over {} ms: {:.4} ({:.1} dBFS), peak {:.4}",
        left.len() * 1000 / RATE as usize,
        rms(&left),
        20.0 * rms(&left).log10(),
        left.iter().fold(0.0f32, |m, s| m.max(s.abs()))
    );
    // Peak and how often the sign changes (a noise measure), per 5 ms.
    for (i, chunk) in left.chunks(240).take(30).enumerate() {
        let peak = chunk.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let flips = chunk.windows(2).filter(|w| (w[0] > 0.0) != (w[1] > 0.0)).count();
        eprintln!("{:4} ms  peak {:.4}  sign changes {:3}", i * 5, peak, flips);
    }
    device.retire();
}
