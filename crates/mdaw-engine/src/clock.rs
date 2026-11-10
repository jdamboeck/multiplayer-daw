//! The clock the engine schedules against.

use std::sync::OnceLock;
use std::time::Instant;

static ORIGIN: OnceLock<Instant> = OnceLock::new();

/// Microseconds on a clock that never jumps, the same for every thread of the process. Only
/// differences mean anything. Safe to call on the audio thread once it has been called anywhere.
pub fn now_micros() -> i64 {
    ORIGIN.get_or_init(Instant::now).elapsed().as_micros() as i64
}
