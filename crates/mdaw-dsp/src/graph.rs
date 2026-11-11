//! The processing graph: every track's devices and events, rendered block by block.
//!
//! `process`, `adopt_from`, `retire` and the note methods never allocate or block, so they may
//! run in the audio callback. Building a graph allocates and happens elsewhere.

use crate::compile::{CompiledKind, CompiledTrack, LIVE_KEY, NoteEvent, sample_at, tick_at};
use crate::external::{BlockTime, ExternalDevice};
use crate::pocket::Pocket;
use mdaw_model::{PPQ, TempoMap, Tick, TrackId};
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
    /// A recording of it plays instead: interleaved stereo at the graph's rate, from the start
    /// of the timeline. For a track this client cannot render.
    Stem(Arc<[f32]>),
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
    /// For a built-in device that follows an external one.
    mono: &'a mut [f32],
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

    fn dispatch(devices: &mut [DeviceState], compiled: &CompiledTrack, event: &NoteEvent) {
        for (state, device) in devices.iter_mut().zip(&compiled.devices) {
            match state {
                DeviceState::Pocket(synth) => {
                    if event.vel == 0 {
                        synth.note_off(event.key);
                    } else if !device.bypass {
                        synth.note_on(event.key, event.pitch, event.vel);
                    }
                }
                DeviceState::External(external) => {
                    if event.vel == 0 {
                        external.note_off(event.key);
                    } else if !device.bypass {
                        external.note_on(event.key, event.pitch, event.vel);
                    }
                }
                DeviceState::Adopt | DeviceState::Inert => {}
            }
        }
    }

    fn render(
        devices: &mut [DeviceState],
        compiled: &CompiledTrack,
        stereo: bool,
        out: &mut Scratch,
        time: &BlockTime,
    ) {
        // Mono: built-in devices add into the left channel, which becomes both in the end.
        let mut split = false;
        for (state, device) in devices.iter_mut().zip(&compiled.devices) {
            match state {
                DeviceState::Pocket(synth) if device.bypass => {
                    // Keep time moving so held notes end, but drop the sound.
                    synth.release_where(|_| true);
                }
                DeviceState::Pocket(synth) if !split => synth.process(out.left),
                DeviceState::Pocket(synth) => {
                    out.mono.fill(0.0);
                    synth.process(out.mono);
                    for ((l, r), m) in out.left.iter_mut().zip(out.right.iter_mut()).zip(out.mono.iter()) {
                        *l += m;
                        *r += m;
                    }
                }
                DeviceState::External(external) => {
                    if !split {
                        out.right.copy_from_slice(out.left);
                        split = true;
                    }
                    if !device.bypass {
                        external.process(out.left, out.right, time);
                    }
                }
                DeviceState::Adopt | DeviceState::Inert => {}
            }
        }
        if stereo && !split {
            out.right.copy_from_slice(out.left);
        }
    }

    /// Renders one block into `out` (zeroed by the caller). `pos` is the timeline sample of the
    /// first frame while the transport runs. A mono track renders into `out.left` only.
    fn process(&mut self, pos: Option<i64>, out: &mut Scratch, time: &BlockTime, sample_rate: u32) {
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
        let mut part = |devices: &mut [DeviceState], from: usize, to: usize| {
            let mut time = *time;
            if from > 0 {
                let seconds = from as f64 / sample_rate as f64;
                time.seconds += seconds;
                if time.playing {
                    time.tick += seconds * time.bpm / 60.0 * PPQ as f64;
                }
            }
            let mut sub = Scratch {
                left: &mut out.left[from..to],
                right: &mut out.right[from..to],
                mono: &mut out.mono[from..to],
            };
            Self::render(devices, compiled, *stereo, &mut sub, &time);
        };
        // Split the block at every event, so notes start on their exact sample and the result
        // does not depend on the block size.
        let mut done = 0;
        for event in events {
            let at = (event.sample - pos.unwrap_or(0)) as usize;
            if at > done {
                part(devices, done, at);
                done = at;
            }
            Self::dispatch(devices, compiled, event);
        }
        if done < frames {
            part(devices, done, frames);
        }
    }

    /// Plays the stem for one block.
    fn play_stem(stem: &[f32], pos: Option<i64>, left: &mut [f32], right: &mut [f32]) {
        let Some(pos) = pos else {
            return;
        };
        let frames = (stem.len() / 2) as i64;
        for (i, (l, r)) in left.iter_mut().zip(right.iter_mut()).enumerate() {
            let at = pos + i as i64;
            if (0..frames).contains(&at) {
                *l = stem[at as usize * 2];
                *r = stem[at as usize * 2 + 1];
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
    mono: Vec<f32>,
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
            mono: vec![0.0; MAX_BLOCK],
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
        let sample_rate = self.sample_rate;
        let mut scratch = Scratch {
            left: &mut self.left[..frames],
            right: &mut self.right[..frames],
            mono: &mut self.mono[..frames],
        };
        for track in &mut self.tracks {
            scratch.left.fill(0.0);
            let stereo = match &track.source {
                Source::Silent => continue,
                Source::Stem(stem) => {
                    scratch.right.fill(0.0);
                    TrackRuntime::play_stem(stem, pos, scratch.left, scratch.right);
                    true
                }
                Source::Devices if track.devices.is_empty() => continue,
                Source::Devices => {
                    track.process(pos, &mut scratch, &time, sample_rate);
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
        }
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
