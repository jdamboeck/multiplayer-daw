//! What the client's test binaries share: one Tracktion engine at a time, and the test synth.
//!
//! Tracktion allows one engine per process. A test that needs one holds [`tracktion`] for the
//! engine's whole life, so the tests of a binary that need it run one after another (an engine
//! can be made again once the previous one is gone); the others, on `SilentAudio`, run
//! alongside.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, Once};

use mdaw_client::audio_tracktion::{TracktionAudio, TracktionOptions};
use mdaw_model::PluginFormat;

static ENGINE: Mutex<()> = Mutex::new(());

/// The right to have a Tracktion engine in this process; hold it until the engine is dropped
/// (declare it before the engine, so that it is dropped after).
pub fn tracktion() -> MutexGuard<'static, ()> {
    ENGINE.lock().unwrap_or_else(|e| e.into_inner())
}

/// A scratch folder under `target/phase5-scratch/<name>`.
pub fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/phase5-scratch")
        .join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Puts a folder with the test synth first on this process's `CLAP_PATH`, once.
pub fn test_synth_on_clap_path() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = mdaw_test_plugin::clap_folder(&scratch("client-tests/clap"));
        // SAFETY: the other tests of these binaries read the environment only through `std`,
        // which takes the same lock as `set_var`; no engine (whose C++ code reads it) exists
        // while the caller holds `tracktion()`.
        unsafe { std::env::set_var("CLAP_PATH", dir) };
    });
}

/// An engine without a device. `scan`: look for CLAP plugins (the test synth among them, see
/// [`test_synth_on_clap_path`]); without, plugins are off. Settings go to a folder per `name`.
pub fn tracktion_audio(name: &str, scan: bool) -> TracktionAudio {
    let mut options = TracktionOptions::new(scratch("client-tests").join(name));
    options.enable_plugins = scan;
    options.scan = if scan {
        vec![PluginFormat::Clap]
    } else {
        Vec::new()
    };
    if scan {
        test_synth_on_clap_path();
    }
    TracktionAudio::new(options).expect("the engine starts")
}
