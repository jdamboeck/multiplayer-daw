//! Turns document tracks into flat, sample-timed data the audio thread can read without
//! allocating or touching the document.

use crate::external::{NoPlugins, PluginRef, PluginSource};
use crate::graph::{DeviceState, Graph, Source, TrackRuntime};
use crate::pocket::{Pocket, PocketParams};
use mdaw_model::{
    BlobHash, BuiltinKind, DeviceId, DeviceKind, PluginFormat, Project, TempoMap, Tick, TrackContent, TrackId,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Keys with this bit set belong to notes played live (keyboard, MIDI, remote players), not to
/// notes from the timeline. Entity IDs never have it set.
pub const LIVE_KEY: u64 = 1 << 63;

/// A note start (`vel > 0`) or end (`vel == 0`) at an absolute timeline sample.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NoteEvent {
    pub sample: i64,
    /// For a note start: the sample of its end. Unused otherwise.
    pub end: i64,
    pub key: u64,
    pub pitch: u8,
    pub vel: u8,
}

#[derive(Clone, PartialEq, Debug)]
pub struct CompiledTrack {
    pub id: TrackId,
    /// Left and right gain: fader, pan and mute folded together.
    pub gain: [f32; 2],
    /// Sorted by sample; at equal samples, note ends come first.
    pub events: Arc<[NoteEvent]>,
    pub devices: Vec<CompiledDevice>,
}

#[derive(Clone, PartialEq, Debug)]
pub struct CompiledDevice {
    pub id: DeviceId,
    pub bypass: bool,
    pub kind: CompiledKind,
}

#[derive(Clone, PartialEq, Debug)]
pub enum CompiledKind {
    Pocket(PocketParams),
    /// A plugin, run through a [`PluginSource`].
    Plugin {
        format: PluginFormat,
        id: String,
        state: Option<BlobHash>,
    },
    /// A device this build cannot render (unknown built-in version).
    Inert,
}

impl CompiledKind {
    /// Tells apart device states that cannot be carried over to one another. A plugin instance
    /// is kept when its state in the document changes: whoever provides plugins decides when a
    /// new state means a new instance (see [`Compiler::forget`]).
    fn tag(&self) -> u64 {
        match self {
            CompiledKind::Inert => 0,
            CompiledKind::Pocket(_) => 1,
            CompiledKind::Plugin { format, id, .. } => {
                // FNV-1a over what identifies the plugin.
                let mut hash = 0xcbf2_9ce4_8422_2325u64;
                for byte in [*format as u8].into_iter().chain(id.bytes()) {
                    hash = (hash ^ byte as u64).wrapping_mul(0x0100_0000_01b3);
                }
                hash | 2
            }
        }
    }
}

/// Timeline position of a tick, in samples.
pub fn sample_at(tempo: &TempoMap, sample_rate: u32, tick: f64) -> i64 {
    (tempo.tick_to_seconds(tick) * sample_rate as f64).round() as i64
}

/// Tick at a timeline sample position.
pub fn tick_at(tempo: &TempoMap, sample_rate: u32, sample: i64) -> f64 {
    tempo.seconds_to_tick(sample as f64 / sample_rate as f64)
}

pub fn compile_events(content: &TrackContent, tempo: &TempoMap, sample_rate: u32) -> Vec<NoteEvent> {
    let mut events = Vec::new();
    for clip in content.clips.values() {
        if clip.length <= Tick::ZERO {
            continue;
        }
        for (id, note) in &clip.notes {
            if note.start < Tick::ZERO || note.start >= clip.length || note.dur <= Tick::ZERO {
                continue;
            }
            // Notes are cut off at the end of their clip.
            let end = (note.start + note.dur).min(clip.length);
            let on = sample_at(tempo, sample_rate, (clip.start + note.start).0 as f64);
            let off = sample_at(tempo, sample_rate, (clip.start + end).0 as f64).max(on + 1);
            let key = id.0.0 & !LIVE_KEY;
            let pitch = note.pitch.min(127);
            events.push(NoteEvent {
                sample: on,
                end: off,
                key,
                pitch,
                vel: note.vel.clamp(1, 127),
            });
            events.push(NoteEvent {
                sample: off,
                end: off,
                key,
                pitch,
                vel: 0,
            });
        }
    }
    events.sort_unstable_by_key(|e| (e.sample, e.vel > 0, e.key));
    events
}

fn pan_gains(gain: f32, pan: f32, mute: bool) -> [f32; 2] {
    if mute || !gain.is_finite() || !pan.is_finite() {
        return [0.0; 2];
    }
    // Equal-power pan law, unity gain in the centre.
    let angle = (pan.clamp(-1.0, 1.0) + 1.0) * core::f32::consts::FRAC_PI_4;
    let gain = gain.clamp(0.0, 4.0) * core::f32::consts::SQRT_2;
    [gain * libm::cosf(angle), gain * libm::sinf(angle)]
}

fn compile_track(
    id: TrackId,
    content: &TrackContent,
    tempo: &TempoMap,
    sample_rate: u32,
    previous: Option<&CompiledTrack>,
) -> CompiledTrack {
    let events = compile_events(content, tempo, sample_rate);
    // Keep the old allocation when the notes did not change: the audio thread uses pointer
    // identity to tell "only a knob moved" from "the notes moved".
    let events = match previous {
        Some(prev) if *prev.events == *events => prev.events.clone(),
        _ => events.into(),
    };
    let devices = content
        .devices
        .iter()
        .map(|device| CompiledDevice {
            id: device.id,
            bypass: device.bypass,
            kind: match &device.kind {
                DeviceKind::Builtin {
                    kind: BuiltinKind::Pocket,
                    dsp_version,
                } if *dsp_version == crate::pocket::DSP_VERSION => {
                    CompiledKind::Pocket(PocketParams::from_map(&device.params))
                }
                DeviceKind::Plugin { format, id, state } => CompiledKind::Plugin {
                    format: *format,
                    id: id.clone(),
                    state: *state,
                },
                DeviceKind::Builtin { .. } => CompiledKind::Inert,
            },
        })
        .collect();
    CompiledTrack {
        id,
        gain: pan_gains(content.gain, content.pan, content.mute),
        events,
        devices,
    }
}

struct Cached {
    content: Arc<TrackContent>,
    compiled: Arc<CompiledTrack>,
}

/// Builds graphs from project snapshots. Remembers the previous build, so unchanged tracks are
/// not recompiled and devices that already exist on the audio thread keep their state there.
pub struct Compiler {
    sample_rate: u32,
    tempo: Arc<TempoMap>,
    tracks: BTreeMap<TrackId, Cached>,
    /// Devices that have state in the graph built last: (track, device, kind tag).
    live: BTreeSet<(TrackId, DeviceId, u64)>,
}

impl Compiler {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            tempo: Arc::new(TempoMap::default()),
            tracks: BTreeMap::new(),
            live: BTreeSet::new(),
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The next graph makes the device anew instead of taking it over from the graph before.
    pub fn forget(&mut self, track: TrackId, device: DeviceId) {
        self.live.retain(|(t, d, _)| (*t, *d) != (track, device));
    }

    /// Builds the graph for a snapshot. `only` limits it to some tracks (offline solo renders).
    ///
    /// The graph expects to replace the one built before it and to take over that graph's
    /// device state through [`Graph::adopt_from`].
    pub fn build(&mut self, project: &Project, only: Option<&[TrackId]>) -> Graph {
        self.build_with(project, only, &mut NoPlugins)
    }

    /// As [`Compiler::build`], with plugins and stems from `source`. A track with a plugin that
    /// `source` cannot provide plays its stem, if `source` has that, and is silent otherwise.
    pub fn build_with(
        &mut self,
        project: &Project,
        only: Option<&[TrackId]>,
        source: &mut dyn PluginSource,
    ) -> Graph {
        let tempo = project.tempo_map();
        if tempo != *self.tempo {
            self.tempo = Arc::new(tempo);
            self.tracks.clear();
        }

        let mut tracks = Vec::new();
        let mut cache = BTreeMap::new();
        let mut live = BTreeSet::new();
        for (id, track) in project.ordered_tracks() {
            if only.is_some_and(|only| !only.contains(&id)) {
                continue;
            }
            let previous = self.tracks.get(&id);
            let compiled = match previous {
                Some(prev) if Arc::ptr_eq(&prev.content, &track.content) => prev.compiled.clone(),
                _ => Arc::new(compile_track(
                    id,
                    &track.content,
                    &self.tempo,
                    self.sample_rate,
                    previous.map(|p| &*p.compiled),
                )),
            };
            let mut missing = false;
            let devices = compiled
                .devices
                .iter()
                .map(|device| {
                    let key = (id, device.id, device.kind.tag());
                    let state = match &device.kind {
                        CompiledKind::Inert => DeviceState::Inert,
                        _ if self.live.contains(&key) => DeviceState::Adopt,
                        CompiledKind::Pocket(params) => {
                            DeviceState::Pocket(Box::new(Pocket::new(self.sample_rate, *params)))
                        }
                        CompiledKind::Plugin {
                            format,
                            id: plugin,
                            state,
                        } => {
                            let plugin = PluginRef {
                                format: *format,
                                id: plugin,
                                state: *state,
                            };
                            match source.device(id, device.id, plugin) {
                                Some(external) => DeviceState::External(external),
                                None => {
                                    missing = true;
                                    DeviceState::Inert
                                }
                            }
                        }
                    };
                    if !matches!(state, DeviceState::Inert) {
                        live.insert(key);
                    }
                    state
                })
                .collect();
            // A plugin that cannot be heard here: the stem stands in for the whole track.
            let sound = match track.stem.as_ref().filter(|_| missing) {
                Some(stem) => source.stem(stem).map_or(Source::Silent, Source::Stem),
                None if missing => Source::Silent,
                None => Source::Devices,
            };
            cache.insert(
                id,
                Cached {
                    content: track.content.clone(),
                    compiled: compiled.clone(),
                },
            );
            tracks.push(TrackRuntime::new(compiled, devices, sound));
        }
        self.tracks = cache;
        self.live = live;
        Graph::new(
            self.sample_rate,
            self.tempo.clone(),
            sanitize_gain(project.master_gain),
            tracks,
        )
    }
}

fn sanitize_gain(gain: f32) -> f32 {
    if gain.is_finite() {
        gain.clamp(0.0, 4.0)
    } else {
        0.0
    }
}
