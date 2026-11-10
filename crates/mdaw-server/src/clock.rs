//! The clock the shared transport runs on.

use std::time::Instant;

/// Microseconds since the server started. Never jumps and never runs backwards, unlike the
/// time of day; it means nothing to anyone but this server process.
#[derive(Clone, Copy, Debug)]
pub struct Clock(Instant);

impl Clock {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(Instant::now())
    }

    pub fn micros(&self) -> i64 {
        self.0.elapsed().as_micros() as i64
    }
}
