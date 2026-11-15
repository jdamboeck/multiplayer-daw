//! A timeline of how edits and plugin settings travel, for finding where time goes. Off unless
//! the environment variable `MDAW_TRACE` is set; then each step is a line on stderr:
//! the time of day to the millisecond, the user's name, and what happened.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static ON: OnceLock<bool> = OnceLock::new();
static NAME: OnceLock<String> = OnceLock::new();

/// Whether tracing is on.
pub fn on() -> bool {
    *ON.get_or_init(|| std::env::var_os("MDAW_TRACE").is_some())
}

/// The name the lines carry, so that two clients writing to one terminal can be told apart.
pub fn set_name(name: &str) {
    let _ = NAME.set(name.to_string());
}

/// Writes a line if tracing is on; `what` is only made then.
pub fn step(what: impl FnOnce() -> String) {
    if !on() {
        return;
    }
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let day = ms % 86_400_000;
    eprintln!(
        "mdaw-trace {:02}:{:02}:{:02}.{:03} {:6} {}",
        day / 3_600_000,
        day / 60_000 % 60,
        day / 1000 % 60,
        day % 1000,
        NAME.get().map_or("", String::as_str),
        what()
    );
}

/// The first bytes of a hash, enough to tell states apart in a trace.
pub fn short(hash: &mdaw_model::BlobHash) -> String {
    hash.0.iter().take(4).map(|b| format!("{b:02x}")).collect()
}
