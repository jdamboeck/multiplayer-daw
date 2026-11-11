//! The shared transport and the datagrams that keep the clients' clocks in step.
//!
//! The transport is session state: it is not part of the document, never stored, and gone when
//! the server restarts. Time is the server's clock in microseconds; every client estimates how
//! far its own clock is from it with [`Datagram::Ping`].

use crate::midi::MidiPacket;
use crate::wire::FrameError;
use mdaw_model::{MAX_TICK, TempoMap, Tick, TickRange};
use serde::{Deserialize, Serialize};

/// How far ahead a start or a jump during playback is scheduled, so the message is everywhere
/// before it is due.
pub const START_LEAD_MICROS: i64 = 150_000;

/// The server pulls anchors that are further in the future than this back in.
pub const MAX_LEAD_MICROS: i64 = 2_000_000;

/// The server pushes anchors of a running transport that are further in the past than this
/// forward. Honest clients anchor at "now" or later.
pub const MAX_LAG_MICROS: i64 = 10_000_000;

/// Where the shared playhead is and whether it moves.
#[derive(Clone, Copy, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct TransportState {
    pub playing: bool,
    /// Stopped: where the playhead rests. Playing: where it is at `anchor_server_micros`.
    pub anchor_tick: f64,
    /// Server time of the anchor. In the future for a start that is still to come; the playhead
    /// waits at `anchor_tick` until then.
    pub anchor_server_micros: i64,
    /// The loop region, if looping is on.
    pub loop_range: Option<TickRange>,
    /// Counts the changes. Assigned by the server; what a client sends is ignored.
    pub epoch: u64,
}

impl TransportState {
    /// The playhead in ticks at server time `server_micros`, for a project with tempo map `tempo`.
    ///
    /// Playback enters the loop region like any other part of the song and wraps at its end. An
    /// anchor at or behind the region's end plays on without wrapping.
    pub fn position(&self, server_micros: i64, tempo: &TempoMap) -> f64 {
        let elapsed = server_micros.saturating_sub(self.anchor_server_micros);
        if !self.playing || elapsed <= 0 {
            return self.anchor_tick;
        }
        let start = tempo.tick_to_seconds(self.anchor_tick);
        let mut at = start + elapsed as f64 / 1e6;
        if let Some(range) = self.loop_range.filter(|r| !r.is_empty()) {
            let from = tempo.tick_to_seconds(range.start.0 as f64);
            let to = tempo.tick_to_seconds(range.end.0 as f64);
            if start < to && at >= to && to > from {
                at = from + (at - from) % (to - from);
            }
        }
        tempo.seconds_to_tick(at)
    }

    /// Whether the playhead is moving at `server_micros`: playing, and the start is not still
    /// to come.
    pub fn running(&self, server_micros: i64) -> bool {
        self.playing && server_micros > self.anchor_server_micros
    }

    /// The same playback described from `server_micros` on: where the playhead is then becomes
    /// the anchor. Used when what lies behind the anchor stops being true, as with a new tempo
    /// (pass the old map) or a new loop region. A transport that is not running is unchanged.
    pub fn reanchored(mut self, server_micros: i64, tempo: &TempoMap) -> Self {
        if self.running(server_micros) {
            self.anchor_tick = self.position(server_micros, tempo);
            self.anchor_server_micros = server_micros;
        }
        self
    }

    /// What the server makes of a state a client asks for at server time `now`: values forced
    /// into range, the next epoch after `current`. `None` if the request makes no sense.
    pub fn accepted(mut self, now: i64, current: &TransportState) -> Option<Self> {
        if !self.anchor_tick.is_finite() {
            return None;
        }
        self.anchor_tick = self.anchor_tick.clamp(0.0, MAX_TICK as f64);
        self.loop_range = self.loop_range.and_then(|range| {
            let start = Tick(range.start.0.clamp(0, MAX_TICK));
            let end = Tick(range.end.0.clamp(0, MAX_TICK));
            (end > start).then_some(TickRange { start, end })
        });
        self.anchor_server_micros = if self.playing {
            self.anchor_server_micros
                .clamp(now - MAX_LAG_MICROS, now + MAX_LEAD_MICROS)
        } else {
            now
        };
        self.epoch = current.epoch + 1;
        Some(self)
    }
}

/// What travels in QUIC datagrams: unreliable, unordered, never retransmitted.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum Datagram {
    /// Client to server. `t0` is the client's own clock and comes back unchanged.
    Ping { t0: i64 },
    /// The server's answer, with its clock at the moment it answered.
    Pong { t0: i64, server_micros: i64 },
    /// Live notes. From a client to the server, which passes them on to the others.
    Midi(MidiPacket),
}

impl Datagram {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("datagrams are plain data")
    }

    /// Trailing bytes are an error.
    pub fn decode(bytes: &[u8]) -> Result<Self, FrameError> {
        crate::wire::decode(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdaw_model::{PPQ, TICKS_PER_BAR, TempoPoint};

    const SECOND: i64 = 1_000_000;

    fn playing(anchor_tick: f64, at: i64) -> TransportState {
        TransportState {
            playing: true,
            anchor_tick,
            anchor_server_micros: at,
            ..TransportState::default()
        }
    }

    #[test]
    fn a_stopped_transport_rests_at_its_anchor() {
        let state = TransportState {
            anchor_tick: 480.0,
            ..TransportState::default()
        };
        assert_eq!(state.position(5 * SECOND, &TempoMap::default()), 480.0);
        assert!(!state.running(5 * SECOND));
    }

    #[test]
    fn a_start_to_come_waits_at_its_anchor() {
        let state = playing(960.0, 10 * SECOND);
        let tempo = TempoMap::default();
        assert_eq!(state.position(9 * SECOND, &tempo), 960.0);
        assert!(!state.running(10 * SECOND));
        // 120 bpm: a beat every half second.
        assert_eq!(
            state.position(10 * SECOND + SECOND / 2, &tempo),
            960.0 + PPQ as f64
        );
        assert!(state.running(10 * SECOND + 1));
    }

    #[test]
    fn playback_follows_the_tempo_map() {
        // One bar at 120 bpm takes 2 s, then 60 bpm: a beat per second.
        let tempo = TempoMap::from_points([
            TempoPoint {
                tick: Tick::ZERO,
                milli_bpm: 120_000,
            },
            TempoPoint {
                tick: Tick::bars(1),
                milli_bpm: 60_000,
            },
        ]);
        let state = playing(0.0, 0);
        assert_eq!(state.position(2 * SECOND, &tempo), TICKS_PER_BAR as f64);
        assert_eq!(state.position(3 * SECOND, &tempo), (TICKS_PER_BAR + PPQ) as f64);
    }

    #[test]
    fn playback_wraps_at_the_end_of_the_loop() {
        let tempo = TempoMap::default();
        let bar = TICKS_PER_BAR as f64;
        let mut state = playing(0.0, 0);
        state.loop_range = Some(TickRange::new(Tick::bars(1), Tick::bars(2)));
        // Bars take 2 s. The playhead enters the loop from before it,
        assert_eq!(state.position(3 * SECOND, &tempo), bar * 1.5);
        // wraps to its start when it reaches the end,
        assert_eq!(state.position(4 * SECOND, &tempo), bar);
        assert_eq!(state.position(5 * SECOND, &tempo), bar * 1.5);
        // and keeps doing so.
        assert_eq!(state.position(9 * SECOND, &tempo), bar * 1.5);

        // Started behind the loop, it never comes back.
        let behind = TransportState {
            anchor_tick: bar * 2.0,
            ..state
        };
        assert_eq!(behind.position(2 * SECOND, &tempo), bar * 3.0);
    }

    #[test]
    fn reanchoring_keeps_the_playhead_where_it_is() {
        let slow = TempoMap::default();
        let fast = TempoMap::from_points([TempoPoint {
            tick: Tick::ZERO,
            milli_bpm: 240_000,
        }]);
        let state = playing(0.0, 0);
        let moved = state.reanchored(SECOND, &slow);
        assert_eq!(moved.anchor_tick, 2.0 * PPQ as f64);
        assert_eq!(moved.anchor_server_micros, SECOND);
        // Under the old tempo nothing changed, under the new one playback goes on from there.
        assert_eq!(
            moved.position(2 * SECOND, &slow),
            state.position(2 * SECOND, &slow)
        );
        assert_eq!(moved.position(2 * SECOND, &fast), 6.0 * PPQ as f64);

        // Nothing to do before the start, or when stopped.
        let waiting = playing(0.0, 5 * SECOND);
        assert_eq!(waiting.reanchored(SECOND, &slow), waiting);
        let stopped = TransportState::default();
        assert_eq!(stopped.reanchored(SECOND, &slow), stopped);
    }

    #[test]
    fn the_server_forces_requests_into_range() {
        let now = 100 * SECOND;
        let current = TransportState {
            epoch: 7,
            ..TransportState::default()
        };
        let wild = TransportState {
            playing: true,
            anchor_tick: -5.0,
            anchor_server_micros: i64::MAX,
            loop_range: Some(TickRange {
                start: Tick(500),
                end: Tick(100),
            }),
            epoch: 999,
        };
        let state = wild.accepted(now, &current).unwrap();
        assert_eq!(state.anchor_tick, 0.0);
        assert_eq!(state.anchor_server_micros, now + MAX_LEAD_MICROS);
        assert_eq!(state.loop_range, None);
        assert_eq!(state.epoch, 8);

        let stale = playing(0.0, 0).accepted(now, &current).unwrap();
        assert_eq!(stale.anchor_server_micros, now - MAX_LAG_MICROS);

        let stopped = TransportState {
            anchor_tick: 1e300,
            anchor_server_micros: 3,
            ..TransportState::default()
        };
        let state = stopped.accepted(now, &current).unwrap();
        assert_eq!(state.anchor_tick, MAX_TICK as f64);
        assert_eq!(state.anchor_server_micros, now);

        assert_eq!(playing(f64::NAN, now).accepted(now, &current), None);
    }

    #[test]
    fn datagrams_round_trip() {
        for datagram in [
            Datagram::Ping { t0: 123_456_789 },
            Datagram::Pong {
                t0: -1,
                server_micros: i64::MAX,
            },
        ] {
            assert_eq!(Datagram::decode(&datagram.encode()).unwrap(), datagram);
        }
        assert!(Datagram::decode(&[]).is_err());
        assert!(Datagram::decode(&[9, 9, 9]).is_err());
    }
}
