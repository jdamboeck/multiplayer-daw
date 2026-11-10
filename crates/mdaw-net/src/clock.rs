//! Estimating the server's clock from pings.

use std::collections::VecDeque;

/// Round trips remembered. With one ping per second, an estimate is at most this many seconds
/// old, which bounds how far the two clocks can have drifted apart since.
const WINDOW: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ClockEstimate {
    /// Server clock minus local clock, in microseconds.
    pub offset_micros: i64,
    /// Round trip time of the ping the estimate comes from. The offset is off by at most half
    /// of it.
    pub rtt_micros: i64,
}

/// Turns ping round trips into an estimate of the server's clock.
///
/// A ping that leaves at local time `t0` and whose answer, stamped `server`, is back at `t1` puts
/// the server's clock at `server` somewhere between `t0` and `t1`. Assuming the middle is off by
/// at most half the round trip, so the fastest round trip of the recent ones is the one to trust:
/// delays only ever add.
#[derive(Default)]
pub struct ClockSync {
    recent: VecDeque<ClockEstimate>,
}

impl ClockSync {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes in one round trip. Returns the estimate if it changed.
    pub fn on_pong(&mut self, t0: i64, server_micros: i64, t1: i64) -> Option<ClockEstimate> {
        let rtt_micros = t1.checked_sub(t0).filter(|rtt| *rtt >= 0)?;
        let before = self.estimate();
        if self.recent.len() == WINDOW {
            self.recent.pop_front();
        }
        self.recent.push_back(ClockEstimate {
            offset_micros: server_micros.saturating_sub(t0 + rtt_micros / 2),
            rtt_micros,
        });
        let after = self.estimate();
        (after != before).then_some(after).flatten()
    }

    /// The best estimate so far.
    pub fn estimate(&self) -> Option<ClockEstimate> {
        self.recent.iter().copied().min_by_key(|sample| sample.rtt_micros)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The server's clock is this far ahead of the local one.
    const AHEAD: i64 = 5_000_000;

    /// A round trip starting at local time `t0`, taking `up` to the server and `down` back.
    fn ping(sync: &mut ClockSync, t0: i64, up: i64, down: i64) -> Option<ClockEstimate> {
        sync.on_pong(t0, t0 + up + AHEAD, t0 + up + down)
    }

    #[test]
    fn a_symmetric_round_trip_gives_the_exact_offset() {
        let mut sync = ClockSync::new();
        assert_eq!(sync.estimate(), None);
        let estimate = ping(&mut sync, 1_000, 300, 300).unwrap();
        assert_eq!(estimate.offset_micros, AHEAD);
        assert_eq!(estimate.rtt_micros, 600);
    }

    #[test]
    fn the_fastest_round_trip_wins() {
        let mut sync = ClockSync::new();
        // A slow, lopsided trip is off by a lot,
        let rough = ping(&mut sync, 0, 40_000, 2_000).unwrap();
        assert_eq!(rough.offset_micros, AHEAD + 19_000);
        // a fast one replaces it,
        let good = ping(&mut sync, 20_000, 200, 400).unwrap();
        assert_eq!(good.offset_micros, AHEAD - 100);
        // and a later slow one changes nothing.
        assert_eq!(ping(&mut sync, 40_000, 9_000, 100), None);
        assert_eq!(sync.estimate(), Some(good));
    }

    #[test]
    fn old_round_trips_are_forgotten() {
        let mut sync = ClockSync::new();
        let best = ping(&mut sync, 0, 10, 10).unwrap();
        for n in 1..WINDOW as i64 {
            assert_eq!(ping(&mut sync, n * 1_000, 500, 500), None);
            assert_eq!(sync.estimate(), Some(best));
        }
        // The fast one drops out of the window; the estimate falls back to what is left.
        let next = ping(&mut sync, 99_000, 500, 700).unwrap();
        assert_eq!(next.rtt_micros, 1_000);
        assert_eq!(next.offset_micros, AHEAD);
    }

    #[test]
    fn answers_from_the_future_are_ignored() {
        let mut sync = ClockSync::new();
        assert_eq!(sync.on_pong(1_000, 0, 999), None);
        assert_eq!(sync.estimate(), None);
    }
}
