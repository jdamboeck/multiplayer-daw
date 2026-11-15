//! The clock recording and the playhead are timed against.

use std::sync::OnceLock;
use std::time::Instant;

static ORIGIN: OnceLock<Instant> = OnceLock::new();

/// Microseconds on a clock that never jumps, the same for every thread of the process. Only
/// differences mean anything.
pub fn now_micros() -> i64 {
    ORIGIN.get_or_init(Instant::now).elapsed().as_micros() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_goes_back() {
        let a = now_micros();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = now_micros();
        assert!(b >= a + 2000, "{a} then {b}");
    }
}
