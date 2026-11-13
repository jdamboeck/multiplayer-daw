//! The processing graph: every track's devices and events, rendered block by block.
//!
//! `process`, `adopt_from`, `retire` and the note methods never allocate or block, so they may
//! run in the audio callback. Building a graph allocates and happens elsewhere.

use crate::compile::{CompiledKind, CompiledTrack, LIVE_KEY, NoteEvent, sample_at, tick_at};
use crate::external::{BlockTime, ExternalDevice};
use crate::pocket::Pocket;
use mdaw_model::{TempoMap, Tick, TrackId};
use std::sync::Arc;

/// Largest number of frames one `process` call accepts.
pub const MAX_BLOCK: usize = 512;

pub enum DeviceState {
    Pocket(Box<Pocket>),
    /// A device the graph does not know, such as a plugin.
    External(Box<dyn ExternalDevice>),
    /// To be filled from the graph this one replaces.
    Adopt,
    Inert,
}

/// What a track sounds like.
pub(crate) enum Source {
    /// Its devices play its notes.
    Devices,
    /// A plugin of the track cannot run here: the track gives no sound.
    Silent,
}

pub struct TrackRuntime {
    compiled: Arc<CompiledTrack>,
    devices: Vec<DeviceState>,
    source: Source,
    /// Some device is external. Such tracks are processed in stereo; the others in mono, as
    /// before external devices existed, so that their output stays bit for bit the same.
    stereo: bool,
}

/// Buffers a track renders into.
struct Scratch<'a> {
    left: &'a mut [f32],
    right: &'a mut [f32],
}

impl TrackRuntime {
    pub(crate) fn new(compiled: Arc<CompiledTrack>, devices: Vec<DeviceState>, source: Source) -> Self {
        let stereo = devices.iter().any(|d| matches!(d, DeviceState::External(_)));
        Self {
            compiled,
            devices,
            source,
            stereo,
        }
    }

    /// Hands a note to the track's devices. External devices get it at `offset` frames into
    /// their next block; with `builtin_only` they are left out.
    fn dispatch(
        devices: &mut [DeviceState],
        compiled: &CompiledTrack,
        event: &NoteEvent,
        offset: u32,
        builtin_only: bool,
    ) {
        for (state, device) in devices.iter_mut().zip(&compiled.devices) {
            match state {
                DeviceState::Pocket(synth) => {
                    if event.vel == 0 {
                        synth.note_off(event.key);
                    } else if !device.bypass {
                        synth.note_on(event.key, event.pitch, event.vel);
                    }
                }
                DeviceState::External(external) if !builtin_only => {
                    if event.vel == 0 {
                        external.note_off(offset, event.key);
                    } else if !device.bypass {
                        external.note_on(offset, event.key, event.pitch, event.vel);
                    }
                }
                DeviceState::External(_) | DeviceState::Adopt | DeviceState::Inert => {}
            }
        }
    }

    /// Renders the built-in devices into `left`, mono.
    fn render(devices: &mut [DeviceState], compiled: &CompiledTrack, left: &mut [f32]) {
        for (state, device) in devices.iter_mut().zip(&compiled.devices) {
            if let DeviceState::Pocket(synth) = state {
                if device.bypass {
                    // Keep time moving so held notes end, but drop the sound.
                    synth.release_where(|_| true);
                } else {
                    synth.process(left);
                }
            }
        }
    }

    /// Renders one block into `out` (zeroed by the caller). `pos` is the timeline sample of the
    /// first frame while the transport runs. A mono track renders into `out.left` only.
    ///
    /// Built-in devices run first, the block split at every event so that notes start on their
    /// exact sample and the result does not depend on the block size. External devices then take
    /// the whole block at once, with the events at their offsets in it: a plugin in another
    /// process is visited once per block, however many notes it holds.
    fn process(&mut self, pos: Option<i64>, out: &mut Scratch, time: &BlockTime) {
        let Self {
            compiled,
            devices,
            stereo,
            ..
        } = self;
        let events: &[NoteEvent] = match pos {
            Some(pos) => {
                let all = &compiled.events;
                let from = all.partition_point(|e| e.sample < pos);
                let to = from + all[from..].partition_point(|e| e.sample < pos + out.left.len() as i64);
                &all[from..to]
            }
            None => &[],
        };
        let frames = out.left.len();
        let mut done = 0;
        for event in events {
            let at = (event.sample - pos.unwrap_or(0)) as usize;
            if at > done {
                Self::render(devices, compiled, &mut out.left[done..at]);
                done = at;
            }
            Self::dispatch(devices, compiled, event, 0, true);
        }
        if done < frames {
            Self::render(devices, compiled, &mut out.left[done..]);
        }
        if !*stereo {
            return;
        }
        out.right.copy_from_slice(out.left);
        for (state, device) in devices.iter_mut().zip(&compiled.devices) {
            let DeviceState::External(external) = state else {
                continue;
            };
            for event in events {
                let offset = (event.sample - pos.unwrap_or(0)) as u32;
                if event.vel == 0 {
                    external.note_off(offset, event.key);
                } else if !device.bypass {
                    external.note_on(offset, event.key, event.pitch, event.vel);
                }
            }
            if !device.bypass {
                external.process(out.left, out.right, time);
            }
        }
    }

    fn release(&mut self, filter: impl Fn(u64) -> bool + Copy) {
        for state in &mut self.devices {
            match state {
                DeviceState::Pocket(synth) => synth.release_where(filter),
                DeviceState::External(external) => external.release_where(&filter),
                DeviceState::Adopt | DeviceState::Inert => {}
            }
        }
    }
}

pub struct Graph {
    sample_rate: u32,
    tempo: Arc<TempoMap>,
    master_gain: f32,
    tracks: Vec<TrackRuntime>,
    left: Vec<f32>,
    right: Vec<f32>,
    /// Until when external devices may take to answer, in this block: the audio callback's
    /// own deadline. `None` offline.
    deadline: Option<std::time::Instant>,
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
            left: vec![0.0; MAX_BLOCK],
            right: vec![0.0; MAX_BLOCK],
            deadline: None,
        }
    }

    /// Whether a device of the graph is external, such as a plugin.
    pub fn has_external(&self) -> bool {
        self.tracks.iter().any(|t| t.stereo)
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
    /// to remap its timeline position. Call [`Graph::retire`] on the old graph afterwards.
    pub fn adopt_from(&mut self, old: &mut Graph) -> bool {
        let tempo_changed = !Arc::ptr_eq(&self.tempo, &old.tempo);
        // Tracks mostly keep their order: each search starts where the last one ended, so this
        // is linear in the number of tracks as long as that holds.
        let mut hint = 0;
        for track in &mut self.tracks {
            let count = old.tracks.len();
            let Some(at) = (0..count)
                .map(|i| (hint + i) % count)
                .find(|&i| old.tracks[i].compiled.id == track.compiled.id)
            else {
                continue;
            };
            hint = at + 1;
            let old_track = &mut old.tracks[at];
            for (state, device) in track.devices.iter_mut().zip(&track.compiled.devices) {
                if !matches!(state, DeviceState::Adopt) {
                    continue;
                }
                let found = old_track
                    .devices
                    .iter_mut()
                    .zip(&old_track.compiled.devices)
                    .find(|(_, d)| d.id == device.id);
                let Some((old_state, _)) = found else {
                    continue;
                };
                match (&mut *old_state, &device.kind) {
                    (DeviceState::Pocket(synth), CompiledKind::Pocket(params)) => {
                        synth.set_params(*params);
                        *state = std::mem::replace(old_state, DeviceState::Inert);
                    }
                    (DeviceState::External(_), CompiledKind::Plugin { .. }) => {
                        *state = std::mem::replace(old_state, DeviceState::Inert);
                    }
                    _ => {}
                }
            }
            track.stereo = track
                .devices
                .iter()
                .any(|d| matches!(d, DeviceState::External(_)));
            if tempo_changed || !Arc::ptr_eq(&track.compiled.events, &old_track.compiled.events) {
                track.release(|key| key & LIVE_KEY == 0);
            }
        }
        tempo_changed
    }

    /// Lets the external devices that were not taken over know that they are done. Call on the
    /// audio thread, on a graph that has been replaced, before it leaves that thread.
    pub fn retire(&mut self) {
        for track in &mut self.tracks {
            for state in &mut track.devices {
                if let DeviceState::External(external) = state {
                    external.retire();
                }
            }
        }
    }

    /// Renders `out.len() / 2` frames of interleaved stereo, at most `MAX_BLOCK`. `pos` is the
    /// timeline sample of the first frame, or `None` while the transport is stopped (live notes
    /// and tails still sound).
    pub fn process(&mut self, pos: Option<i64>, out: &mut [f32]) {
        let frames = out.len() / 2;
        debug_assert!(frames <= MAX_BLOCK && out.len().is_multiple_of(2));
        let frames = frames.min(MAX_BLOCK);
        out.fill(0.0);
        let time = self.time(pos);
        let mut scratch = Scratch {
            left: &mut self.left[..frames],
            right: &mut self.right[..frames],
        };
        for track in &mut self.tracks {
            scratch.left.fill(0.0);
            let stereo = match &track.source {
                Source::Silent => continue,
                Source::Devices if track.devices.is_empty() => continue,
                Source::Devices => {
                    track.process(pos, &mut scratch, &time);
                    track.stereo
                }
            };
            let [gain_left, gain_right] = track.compiled.gain;
            let right: &[f32] = if stereo { scratch.right } else { scratch.left };
            let frames = out.as_chunks_mut::<2>().0.iter_mut();
            for ((frame, left), right) in frames.zip(scratch.left.iter()).zip(right.iter()) {
                frame[0] += left * gain_left;
                frame[1] += right * gain_right;
            }
        }
        if self.master_gain != 1.0 {
            for sample in out.iter_mut() {
                *sample *= self.master_gain;
            }
        }
    }

    fn time(&self, pos: Option<i64>) -> BlockTime {
        let sample = pos.unwrap_or(0);
        let tick = self.tick_at(sample);
        BlockTime {
            playing: pos.is_some(),
            tick,
            seconds: sample as f64 / self.sample_rate as f64,
            bpm: self.tempo.milli_bpm_at(Tick(tick.max(0.0) as i64)) as f64 / 1000.0,
            deadline: self.deadline,
        }
    }

    /// Sets until when external devices may take to answer in the blocks to come: what is left
    /// of the audio callback. They share it; one that is later goes without its sound for the
    /// block, so that the callback is never late.
    pub fn set_deadline(&mut self, deadline: Option<std::time::Instant>) {
        self.deadline = deadline;
    }

    /// Starts the timeline notes that are already sounding at `pos`: those that began before it
    /// and end after it. Call when playback starts or jumps there, before processing.
    pub fn chase(&mut self, pos: i64) {
        for track in &mut self.tracks {
            let TrackRuntime {
                compiled, devices, ..
            } = track;
            let before = compiled.events.partition_point(|e| e.sample < pos);
            for event in &compiled.events[..before] {
                if event.vel > 0 && event.end > pos {
                    TrackRuntime::dispatch(devices, compiled, event, 0, false);
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
            TrackRuntime::dispatch(&mut t.devices, &t.compiled, &event, 0, false);
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
            TrackRuntime::dispatch(&mut t.devices, &t.compiled, &event, 0, false);
        }
    }
}
