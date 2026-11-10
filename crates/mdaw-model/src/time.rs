use crate::ids::TempoId;
use imbl::OrdMap;
use serde::{Deserialize, Serialize};

/// Ticks per quarter note.
pub const PPQ: i64 = 960;
/// The prototype has no time signatures: every bar is 4/4.
pub const TICKS_PER_BAR: i64 = PPQ * 4;
/// Upper bound for any position or length (100 000 bars).
pub const MAX_TICK: i64 = TICKS_PER_BAR * 100_000;

pub const DEFAULT_MILLI_BPM: u32 = 120_000;
pub const MIN_MILLI_BPM: u32 = 20_000;
pub const MAX_MILLI_BPM: u32 = 400_000;

/// Musical time at 960 PPQ. Clip positions are absolute; note positions are relative to the clip.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default, Serialize, Deserialize)]
pub struct Tick(pub i64);

impl Tick {
    pub const ZERO: Tick = Tick(0);
    pub const fn bars(n: i64) -> Tick {
        Tick(n * TICKS_PER_BAR)
    }
    pub const fn beats(n: i64) -> Tick {
        Tick(n * PPQ)
    }
}

impl std::ops::Add for Tick {
    type Output = Tick;
    fn add(self, rhs: Tick) -> Tick {
        Tick(self.0.saturating_add(rhs.0))
    }
}

impl std::ops::Sub for Tick {
    type Output = Tick;
    fn sub(self, rhs: Tick) -> Tick {
        Tick(self.0.saturating_sub(rhs.0))
    }
}

/// Half-open range `[start, end)`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct TickRange {
    pub start: Tick,
    pub end: Tick,
}

impl TickRange {
    pub fn new(start: Tick, end: Tick) -> Self {
        Self {
            start,
            end: end.max(start),
        }
    }
    pub fn len(&self) -> Tick {
        self.end - self.start
    }
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
    pub fn union(self, other: TickRange) -> TickRange {
        TickRange {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
    pub fn overlaps(&self, other: &TickRange) -> bool {
        self.start < other.end && other.start < self.end
    }
    /// Grows the range outwards to whole bars. An empty range becomes one bar.
    pub fn snap_to_bars(self) -> TickRange {
        let start = self.start.0.div_euclid(TICKS_PER_BAR) * TICKS_PER_BAR;
        let end = (self.end.0.saturating_add(TICKS_PER_BAR - 1)).div_euclid(TICKS_PER_BAR) * TICKS_PER_BAR;
        TickRange {
            start: Tick(start),
            end: Tick(end.max(start + TICKS_PER_BAR)),
        }
    }
}

/// From `tick` on, the tempo is `milli_bpm / 1000` quarter notes per minute.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct TempoPoint {
    pub tick: Tick,
    pub milli_bpm: u32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct Segment {
    tick: i64,
    seconds: f64,
    seconds_per_tick: f64,
    milli_bpm: u32,
}

/// The project's tempo points compiled for conversion between ticks and seconds.
///
/// Uses only `f64` addition, multiplication and division, which are exactly specified by IEEE 754,
/// so every replica computes identical results.
#[derive(Clone, PartialEq, Debug)]
pub struct TempoMap {
    segments: Vec<Segment>,
}

impl Default for TempoMap {
    fn default() -> Self {
        Self::from_points(std::iter::empty())
    }
}

impl TempoMap {
    /// Compiles the document's tempo points. Points before tick 0 count as tick 0. When several
    /// points share a tick, the one with the highest ID wins. Without a point at tick 0 the map
    /// starts at 120 bpm.
    pub fn compile(points: &OrdMap<TempoId, TempoPoint>) -> Self {
        // OrdMap iterates in ID order, so a stable sort by tick leaves the highest ID last.
        Self::from_points(points.values().copied())
    }

    pub fn from_points(points: impl IntoIterator<Item = TempoPoint>) -> Self {
        let mut sorted: Vec<(i64, u32)> = points
            .into_iter()
            .map(|p| {
                (
                    p.tick.0.clamp(0, MAX_TICK),
                    p.milli_bpm.clamp(MIN_MILLI_BPM, MAX_MILLI_BPM),
                )
            })
            .collect();
        sorted.sort_by_key(|&(tick, _)| tick);

        let mut flat: Vec<(i64, u32)> = vec![(0, DEFAULT_MILLI_BPM)];
        for (tick, milli_bpm) in sorted {
            match flat.last_mut() {
                Some(last) if last.0 == tick => last.1 = milli_bpm,
                _ => flat.push((tick, milli_bpm)),
            }
        }

        let mut segments: Vec<Segment> = Vec::with_capacity(flat.len());
        for (tick, milli_bpm) in flat {
            let seconds = segments
                .last()
                .map_or(0.0, |s| s.seconds + (tick - s.tick) as f64 * s.seconds_per_tick);
            let seconds_per_tick = 60_000.0 / (milli_bpm as f64 * PPQ as f64);
            segments.push(Segment {
                tick,
                seconds,
                seconds_per_tick,
                milli_bpm,
            });
        }
        Self { segments }
    }

    fn segment_at_tick(&self, tick: f64) -> &Segment {
        let idx = self.segments.partition_point(|s| (s.tick as f64) <= tick);
        &self.segments[idx.saturating_sub(1)]
    }

    fn segment_at_seconds(&self, seconds: f64) -> &Segment {
        let idx = self.segments.partition_point(|s| s.seconds <= seconds);
        &self.segments[idx.saturating_sub(1)]
    }

    /// Seconds from tick 0. Negative ticks extrapolate with the first tempo.
    pub fn tick_to_seconds(&self, tick: f64) -> f64 {
        let s = self.segment_at_tick(tick);
        s.seconds + (tick - s.tick as f64) * s.seconds_per_tick
    }

    pub fn seconds_to_tick(&self, seconds: f64) -> f64 {
        let s = self.segment_at_seconds(seconds);
        s.tick as f64 + (seconds - s.seconds) / s.seconds_per_tick
    }

    pub fn milli_bpm_at(&self, tick: Tick) -> u32 {
        self.segment_at_tick(tick.0 as f64).milli_bpm
    }

    /// The effective points, sorted, starting at tick 0.
    pub fn points(&self) -> impl Iterator<Item = TempoPoint> + '_ {
        self.segments.iter().map(|s| TempoPoint {
            tick: Tick(s.tick),
            milli_bpm: s.milli_bpm,
        })
    }

    /// Identifies the timing a stem was rendered against (FNV-1a over the effective points).
    pub fn hash(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |bytes: &[u8]| {
            for &b in bytes {
                h ^= b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        for s in &self.segments {
            eat(&s.tick.to_le_bytes());
            eat(&s.milli_bpm.to_le_bytes());
        }
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(tick: i64, bpm: u32) -> TempoPoint {
        TempoPoint {
            tick: Tick(tick),
            milli_bpm: bpm * 1000,
        }
    }

    #[test]
    fn default_is_120_bpm() {
        let map = TempoMap::default();
        assert_eq!(map.tick_to_seconds(PPQ as f64), 0.5);
        assert_eq!(map.seconds_to_tick(2.0), TICKS_PER_BAR as f64);
        assert_eq!(map.tick_to_seconds(-(PPQ as f64)), -0.5);
    }

    #[test]
    fn tempo_change_mid_song() {
        // One bar at 120 bpm (2 s), then 60 bpm (1 s per beat).
        let map = TempoMap::from_points([point(0, 120), point(TICKS_PER_BAR, 60)]);
        assert_eq!(map.tick_to_seconds(TICKS_PER_BAR as f64), 2.0);
        assert_eq!(map.tick_to_seconds((TICKS_PER_BAR + PPQ) as f64), 3.0);
        assert_eq!(map.seconds_to_tick(3.0), (TICKS_PER_BAR + PPQ) as f64);
        assert_eq!(map.milli_bpm_at(Tick(TICKS_PER_BAR - 1)), 120_000);
        assert_eq!(map.milli_bpm_at(Tick(TICKS_PER_BAR)), 60_000);
    }

    #[test]
    fn same_tick_last_wins_and_hash_follows_effective_points() {
        let a = TempoMap::from_points([point(0, 100), point(0, 140)]);
        let b = TempoMap::from_points([point(0, 140)]);
        assert_eq!(a, b);
        assert_eq!(a.hash(), b.hash());
        assert_ne!(a.hash(), TempoMap::default().hash());
    }

    #[test]
    fn out_of_range_tempo_is_clamped() {
        let map = TempoMap::from_points([TempoPoint {
            tick: Tick(0),
            milli_bpm: 0,
        }]);
        assert!(map.tick_to_seconds(960.0).is_finite());
        assert_eq!(map.milli_bpm_at(Tick(0)), MIN_MILLI_BPM);
    }

    #[test]
    fn snapping() {
        let r = TickRange::new(Tick(100), Tick(TICKS_PER_BAR + 1)).snap_to_bars();
        assert_eq!(r, TickRange::new(Tick(0), Tick::bars(2)));
        let empty = TickRange::new(Tick::bars(3), Tick::bars(3)).snap_to_bars();
        assert_eq!(empty, TickRange::new(Tick::bars(3), Tick::bars(4)));
    }
}
