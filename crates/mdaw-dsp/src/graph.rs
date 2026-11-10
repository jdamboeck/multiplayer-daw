//! The processing graph: every track's devices and events, rendered block by block.
//!
//! `process`, `adopt_from` and the note methods never allocate or block, so they may run in the
//! audio callback. Building a graph allocates and happens elsewhere.

use crate::compile::{CompiledKind, CompiledTrack, LIVE_KEY, NoteEvent, sample_at, tick_at};
use crate::pocket::Pocket;
use mdaw_model::{TempoMap, TrackId};
use std::sync::Arc;

/// Largest number of frames one `process` call accepts.
pub const MAX_BLOCK: usize = 512;

pub enum DeviceState {
    Pocket(Box<Pocket>),
    /// To be filled from the graph this one replaces.
    Adopt,
    Inert,
}

pub struct TrackRuntime {
    compiled: Arc<CompiledTrack>,
    devices: Vec<DeviceState>,
}

impl TrackRuntime {
    pub(crate) fn new(compiled: Arc<CompiledTrack>, devices: Vec<DeviceState>) -> Self {
        Self { compiled, devices }
    }

    fn dispatch(devices: &mut [DeviceState], compiled: &CompiledTrack, event: &NoteEvent) {
        for (state, device) in devices.iter_mut().zip(&compiled.devices) {
            if let DeviceState::Pocket(synth) = state {
                if event.vel == 0 {
                    synth.note_off(event.key);
                } else if !device.bypass {
                    synth.note_on(event.key, event.pitch, event.vel);
                }
            }
        }
    }

    fn render(devices: &mut [DeviceState], compiled: &CompiledTrack, buf: &mut [f32]) {
        for (state, device) in devices.iter_mut().zip(&compiled.devices) {
            if let DeviceState::Pocket(synth) = state {
                if device.bypass {
                    // Keep time moving so held notes end, but drop the sound.
                    synth.release_where(|_| true);
                } else {
                    synth.process(buf);
                }
            }
        }
    }

    /// Renders one block into `buf` (mono, zeroed by the caller). `pos` is the timeline sample
    /// of the first frame while the transport runs.
    fn process(&mut self, pos: Option<i64>, buf: &mut [f32]) {
        let Self { compiled, devices } = self;
        let events: &[NoteEvent] = match pos {
            Some(pos) => {
                let all = &compiled.events;
                let from = all.partition_point(|e| e.sample < pos);
                let to = from + all[from..].partition_point(|e| e.sample < pos + buf.len() as i64);
                &all[from..to]
            }
            None => &[],
        };
        // Split the block at every event, so notes start on their exact sample and the result
        // does not depend on the block size.
        let mut done = 0;
        for event in events {
            let at = (event.sample - pos.unwrap_or(0)) as usize;
            if at > done {
                Self::render(devices, compiled, &mut buf[done..at]);
                done = at;
            }
            Self::dispatch(devices, compiled, event);
        }
        if done < buf.len() {
            Self::render(devices, compiled, &mut buf[done..]);
        }
    }

    fn release(&mut self, filter: impl Fn(u64) -> bool + Copy) {
        for state in &mut self.devices {
            if let DeviceState::Pocket(synth) = state {
                synth.release_where(filter);
            }
        }
    }
}

pub struct Graph {
    sample_rate: u32,
    tempo: Arc<TempoMap>,
    master_gain: f32,
    tracks: Vec<TrackRuntime>,
    buf: Vec<f32>,
}

impl Graph {
    pub(crate) fn new(
        sample_rate: u32,
        tempo: Arc<TempoMap>,
        master_gain: f32,
        tracks: Vec<TrackRuntime>,
    ) -> Self {
        Self {
            sample_rate,
            tempo,
            master_gain,
            tracks,
            buf: vec![0.0; MAX_BLOCK],
        }
    }

    /// A graph with no tracks.
    pub fn empty(sample_rate: u32) -> Self {
        Self::new(sample_rate, Arc::new(TempoMap::default()), 1.0, Vec::new())
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn tempo(&self) -> &TempoMap {
        &self.tempo
    }

    /// Timeline sample of a tick under this graph's tempo map.
    pub fn sample_at(&self, tick: f64) -> i64 {
        sample_at(&self.tempo, self.sample_rate, tick)
    }

    pub fn tick_at(&self, sample: i64) -> f64 {
        tick_at(&self.tempo, self.sample_rate, sample)
    }

    /// Takes over the device state of the graph this one replaces, so sounding notes carry on.
    /// Notes from the timeline are released on tracks whose notes changed: their note-offs may
    /// no longer exist. Returns true when the tempo map differs, in which case the caller has
    /// to remap its timeline position.
    pub fn adopt_from(&mut self, old: &mut Graph) -> bool {
        let tempo_changed = !Arc::ptr_eq(&self.tempo, &old.tempo);
        for track in &mut self.tracks {
            let Some(old_track) = old.tracks.iter_mut().find(|t| t.compiled.id == track.compiled.id) else {
                continue;
            };
            for (state, device) in track.devices.iter_mut().zip(&track.compiled.devices) {
                if !matches!(state, DeviceState::Adopt) {
                    continue;
                }
                let found = old_track
                    .devices
                    .iter_mut()
                    .zip(&old_track.compiled.devices)
                    .find(|(_, d)| d.id == device.id);
                if let Some((old_state, _)) = found
                    && let (DeviceState::Pocket(synth), CompiledKind::Pocket(params)) =
                        (&mut *old_state, &device.kind)
                {
                    synth.set_params(*params);
                    *state = std::mem::replace(old_state, DeviceState::Inert);
                }
            }
            if tempo_changed || !Arc::ptr_eq(&track.compiled.events, &old_track.compiled.events) {
                track.release(|key| key & LIVE_KEY == 0);
            }
        }
        tempo_changed
    }

    /// Renders `out.len() / 2` frames of interleaved stereo, at most `MAX_BLOCK`. `pos` is the
    /// timeline sample of the first frame, or `None` while the transport is stopped (live notes
    /// and tails still sound).
    pub fn process(&mut self, pos: Option<i64>, out: &mut [f32]) {
        let frames = out.len() / 2;
        debug_assert!(frames <= MAX_BLOCK && out.len().is_multiple_of(2));
        let frames = frames.min(MAX_BLOCK);
        out.fill(0.0);
        let buf = &mut self.buf[..frames];
        for track in &mut self.tracks {
            if track.devices.is_empty() {
                continue;
            }
            buf.fill(0.0);
            track.process(pos, buf);
            let [left, right] = track.compiled.gain;
            for (frame, sample) in out.chunks_exact_mut(2).zip(buf.iter()) {
                frame[0] += sample * left;
                frame[1] += sample * right;
            }
        }
        if self.master_gain != 1.0 {
            for sample in out.iter_mut() {
                *sample *= self.master_gain;
            }
        }
    }

    /// Starts the timeline notes that are already sounding at `pos`: those that began before it
    /// and end after it. Call when playback starts or jumps there, before processing.
    pub fn chase(&mut self, pos: i64) {
        for track in &mut self.tracks {
            let TrackRuntime { compiled, devices } = track;
            let before = compiled.events.partition_point(|e| e.sample < pos);
            for event in &compiled.events[..before] {
                if event.vel > 0 && event.end > pos {
                    TrackRuntime::dispatch(devices, compiled, event);
                }
            }
        }
    }

    /// Ends all notes that came from the timeline. Call on stop, seek and loop jumps.
    pub fn release_timeline_notes(&mut self) {
        for track in &mut self.tracks {
            track.release(|key| key & LIVE_KEY == 0);
        }
    }

    /// Ends every note, live ones included.
    pub fn release_all_notes(&mut self) {
        for track in &mut self.tracks {
            track.release(|_| true);
        }
    }

    /// Starts a live note on a track. `LIVE_KEY` is added to the key.
    pub fn note_on(&mut self, track: TrackId, key: u64, pitch: u8, vel: u8) {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.compiled.id == track) {
            let event = NoteEvent {
                sample: 0,
                end: 0,
                key: key | LIVE_KEY,
                pitch,
                vel: vel.max(1),
            };
            TrackRuntime::dispatch(&mut t.devices, &t.compiled, &event);
        }
    }

    pub fn note_off(&mut self, track: TrackId, key: u64) {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.compiled.id == track) {
            let event = NoteEvent {
                sample: 0,
                end: 0,
                key: key | LIVE_KEY,
                pitch: 0,
                vel: 0,
            };
            TrackRuntime::dispatch(&mut t.devices, &t.compiled, &event);
        }
    }
}
