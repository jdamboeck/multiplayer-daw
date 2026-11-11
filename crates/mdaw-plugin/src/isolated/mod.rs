//! Plugins in helper processes, so that a plugin that crashes or hangs never takes the DAW
//! with it. The helper is `mdaw-plugin-host`, which runs one instance; it is found next to the
//! running program, or where `MDAW_PLUGIN_HOST` says.
//!
//! Control (state, parameters, the plugin's window) goes over the helper's stdin and stdout.
//! Audio goes through a block of shared memory, handed back and forth within the audio
//! callback, so it adds no latency; a helper that does not answer in time costs only its own
//! sound for that block. The plugin's window belongs to the helper, whose main loop runs all
//! the time.

pub mod child;
mod parent;
mod protocol;
mod shm;

pub use parent::IsolatedBackend;

use std::path::PathBuf;

/// The helper's file name.
pub const HELPER: &str = if cfg!(windows) {
    "mdaw-plugin-host.exe"
} else {
    "mdaw-plugin-host"
};

/// Where the helper is: `MDAW_PLUGIN_HOST`, or beside the running program (or one folder up,
/// as for test binaries in `target/debug/deps`).
pub fn helper() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("MDAW_PLUGIN_HOST") {
        return Some(path.into());
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [dir.join(HELPER), dir.parent()?.join(HELPER)]
        .into_iter()
        .find(|p| p.is_file())
}
