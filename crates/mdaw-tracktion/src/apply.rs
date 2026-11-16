//! The engine's project, derived from the canonical model (ADR 0001).
//!
//! [`load_project`] clears the engine and builds everything from a project;
//! [`crate::sync_project`] brings it from one state of the project to another by their
//! difference. Two small functions put in a single device: [`add_device`] a device the engine
//! lacks (a plugin that was not installed, or failed, at the last load or sync), and
//! [`restore_device_state`] a plugin's state once its blob has arrived. All of them read the
//! values to send from the project they are given, so calling them again leaves the engine as
//! the model is. The engine addresses everything by the model's own ids.
//!
//! What the engine cannot take does not fail the call: it is left out and reported as a
//! [`Problem`]. Model data the engine has no use for (project name, track colour,
//! stems) is ignored without a report.
//!
//! A plugin device's settings are its state blob. The built-in instrument's are its parameter
//! values.
//!
//! The built-in instrument (`BuiltinKind::TwoOsc`) is played by the engine's own synth,
//! Tracktion's 4osc: it is a front for 4osc whose values are 4osc's own (see
//! [`crate::two_osc`]).

use crate::two_osc::{self as two_osc_map, Target};
use crate::{Engine, EntityKind, Error, Note as EngineNote, PluginInfo, Status};
use mdaw_model::two_osc::Values as TwoOscValues;
use mdaw_model::{
    BlobHash, BuiltinKind, ClipId, Device, DeviceId, DeviceKind, MAX_MILLI_BPM, MAX_TICK, MIN_MILLI_BPM,
    Note, NoteId, PluginFormat, Project, TempoId, TrackContent, TrackId,
};
use std::collections::BTreeMap;
use std::fmt;

/// Gives the bytes of a blob (a plugin's saved state) by its hash, if they are at hand.
pub type BlobResolver<'a> = &'a dyn Fn(&BlobHash) -> Option<Vec<u8>>;

/// The engine's own tempo range (Tracktion's `TempoSetting`), in thousandths of a bpm.
const ENGINE_MAX_MILLI_BPM: u32 = 300_000;
const ENGINE_MIN_MILLI_BPM: u32 = 20_000;

/// Something in the model the engine did not take. The rest was applied.
#[derive(Clone, PartialEq, Debug)]
pub enum Problem {
    /// A plugin device left out: its plugin is not installed (or not scanned), or failed to
    /// load. Ops addressed to it do nothing in the engine.
    DeviceSkipped {
        track: TrackId,
        device: DeviceId,
        format: PluginFormat,
        plugin: String,
        reason: String,
    },
    /// A parameter the built-in instrument does not have.
    ParamNotMapped {
        track: TrackId,
        device: DeviceId,
        param: u32,
    },
    /// A plugin's saved state is not at hand yet (its blob has to be fetched). The device runs
    /// with the settings it has.
    StateMissing {
        track: TrackId,
        device: DeviceId,
        blob: BlobHash,
    },
    /// A plugin refused its saved state.
    StateNotApplied {
        track: TrackId,
        device: DeviceId,
        blob: BlobHash,
        reason: String,
    },
    /// A tempo outside the engine's range (20..300 bpm); `used` was sent instead. Since the
    /// model's own range is the engine's (`mdaw_model::MAX_MILLI_BPM`), only a project that
    /// would fail `Op::validate` can cause it; kept as a guard against the two drifting apart.
    TempoClamped { id: TempoId, milli_bpm: u32, used: u32 },
    /// A note the engine refuses (pitch over 127, velocity 0, length under 1, negative start).
    NoteSkipped {
        track: TrackId,
        clip: ClipId,
        note: NoteId,
    },
    /// A clip the engine refuses (length under 1). Its notes are left out with it.
    ClipSkipped { track: TrackId, clip: ClipId },
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::DeviceSkipped {
                device,
                format,
                plugin,
                reason,
                ..
            } => write!(
                f,
                "{device:?}: {} plugin {plugin} left out: {reason}",
                format.name()
            ),
            Problem::ParamNotMapped { device, param, .. } => {
                write!(f, "{device:?}: parameter {param} cannot be given to the engine")
            }
            Problem::StateMissing { device, blob, .. } => {
                write!(f, "{device:?}: state {blob:?} is not at hand")
            }
            Problem::StateNotApplied {
                device, blob, reason, ..
            } => write!(f, "{device:?}: state {blob:?} not restored: {reason}"),
            Problem::TempoClamped { id, milli_bpm, used } => {
                write!(
                    f,
                    "{id:?}: {milli_bpm} milli-bpm is outside the engine's range; {used} used"
                )
            }
            Problem::NoteSkipped { note, .. } => write!(f, "{note:?}: values out of the engine's range"),
            Problem::ClipSkipped { clip, .. } => write!(f, "{clip:?}: length out of the engine's range"),
        }
    }
}

/// Clears the engine and builds it from `project`.
///
/// `blobs` gives plugin states by hash. Returns what could not be carried over; a missing
/// plugin does not fail the load. Errors are engine failures the model cannot cause (and a
/// project that would fail `Op::validate`, such as a negative gain).
pub fn load_project(
    engine: &Engine,
    project: &Project,
    blobs: BlobResolver<'_>,
) -> Result<Vec<Problem>, Error> {
    let mut sync = Sync::new(engine, blobs);
    engine.clear()?;
    engine.master_set_gain(project.master_gain)?;
    for (tick, (id, milli_bpm)) in effective_tempo(project) {
        sync.put_tempo(id, tick, milli_bpm)?;
    }
    let mut previous = 0;
    for (id, _) in ordered_tracks(project) {
        sync.put_track(project, id, previous)?;
        previous = raw_track(id);
    }
    Ok(sync.problems)
}

/// Puts a device of `project` that the engine lacks into it, behind its nearest left
/// neighbour the engine has: a plugin that was not installed, or failed, when the project
/// was last brought to the engine. Does nothing if the project has no such device or the
/// engine has it already.
pub fn add_device(
    engine: &Engine,
    project: &Project,
    track: TrackId,
    device: DeviceId,
    blobs: BlobResolver<'_>,
) -> Result<Vec<Problem>, Error> {
    let mut sync = Sync::new(engine, blobs);
    if let Some(c) = content(project, track)
        && let Some(found) = c.device(device)
        && !engine.has(EntityKind::Device, raw_device(device))
    {
        let behind = sync.device_predecessor(c, device);
        sync.put_device(track, found, behind)?;
    }
    Ok(sync.problems)
}

/// Gives a plugin device the state `project` holds for it (its blob has arrived). Does nothing
/// for a built-in device or one the engine lacks.
pub fn restore_device_state(
    engine: &Engine,
    project: &Project,
    track: TrackId,
    device: DeviceId,
    blobs: BlobResolver<'_>,
) -> Result<Vec<Problem>, Error> {
    let mut sync = Sync::new(engine, blobs);
    let Some(found) = content(project, track).and_then(|c| c.device(device)) else {
        return Ok(sync.problems);
    };
    let DeviceKind::Plugin { state, .. } = &found.kind else {
        // The model ignores state for built-in devices.
        return Ok(sync.problems);
    };
    if !engine.has(EntityKind::Device, raw_device(device)) {
        return Ok(sync.problems);
    }
    match state {
        Some(blob) => sync.put_state(track, device, *blob),
        // No saved state: the plugin as it starts.
        None => engine.device_reset_state(raw_device(device))?,
    }
    Ok(sync.problems)
}

/// Finds a model plugin in the engine's list: the engine's identifier is the plugin's own id
/// (for CLAP the CLAP id), which is what the model stores.
pub fn find_plugin<'a>(plugins: &'a [PluginInfo], format: PluginFormat, id: &str) -> Option<&'a PluginInfo> {
    plugins
        .iter()
        .find(|p| p.format == format.name() && p.identifier == id)
}

// ---- ids ---------------------------------------------------------------------------------------

pub(crate) fn raw_track(id: TrackId) -> u64 {
    id.0.0
}
pub(crate) fn raw_clip(id: ClipId) -> u64 {
    id.0.0
}
pub(crate) fn raw_note(id: NoteId) -> u64 {
    id.0.0
}
pub(crate) fn raw_device(id: DeviceId) -> u64 {
    id.0.0
}
pub(crate) fn raw_tempo(id: TempoId) -> u64 {
    id.0.0
}

// ---- reading the model -------------------------------------------------------------------------

/// Tracks in display order, then any the order does not list (only a hand-built document has
/// those).
pub(crate) fn ordered_tracks(project: &Project) -> Vec<(TrackId, &TrackContent)> {
    let mut out: Vec<(TrackId, &TrackContent)> = project
        .ordered_tracks()
        .map(|(id, t)| (id, t.content.as_ref()))
        .collect();
    for (id, t) in &project.tracks {
        if !project.track_order.contains(id) {
            out.push((*id, t.content.as_ref()));
        }
    }
    out
}

/// The tempo map as the model compiles it: points before 0 count as 0, and of points at one
/// tick the highest id wins. The engine keeps one point per tick, so it gets the winners only.
pub(crate) fn effective_tempo(project: &Project) -> BTreeMap<i64, (TempoId, u32)> {
    let mut points = BTreeMap::new();
    // OrdMap iterates in id order, so a later (higher) id replaces an earlier one.
    for (id, point) in &project.tempo {
        points.insert(tempo_tick(point.tick.0), (*id, point.milli_bpm));
    }
    points
}

fn tempo_tick(tick: i64) -> i64 {
    tick.clamp(0, MAX_TICK)
}

fn engine_note(id: NoteId, note: &Note) -> Option<EngineNote> {
    let valid = note.start.0 >= 0 && note.dur.0 >= 1 && note.pitch <= 127 && (1..=127).contains(&note.vel);
    valid.then_some(EngineNote {
        id: raw_note(id),
        start_tick: note.start.0,
        length_ticks: note.dur.0,
        pitch: note.pitch,
        velocity: note.vel,
    })
}

pub(crate) fn content(project: &Project, track: TrackId) -> Option<&TrackContent> {
    project.track(track).map(|t| t.content.as_ref())
}

// ---- writing the engine ------------------------------------------------------------------------

pub(crate) struct Sync<'a> {
    pub(crate) engine: &'a Engine,
    blobs: BlobResolver<'a>,
    pub(crate) problems: Vec<Problem>,
    /// The engine's plugin list, read once per call when a plugin device is first met.
    plugins: Option<Vec<PluginInfo>>,
}

impl<'a> Sync<'a> {
    pub(crate) fn new(engine: &'a Engine, blobs: BlobResolver<'a>) -> Self {
        Self {
            engine,
            blobs,
            problems: Vec::new(),
            plugins: None,
        }
    }

    // ---- tempo

    pub(crate) fn put_tempo(&mut self, id: TempoId, tick: i64, milli_bpm: u32) -> Result<(), Error> {
        // The model clamps to its own range when it compiles the map. That range is the
        // engine's now, so the second clamp changes nothing for a valid project; it stays in
        // case the two ever differ again.
        let model = milli_bpm.clamp(MIN_MILLI_BPM, MAX_MILLI_BPM);
        let used = model.clamp(ENGINE_MIN_MILLI_BPM, ENGINE_MAX_MILLI_BPM);
        if used != model {
            self.problems.push(Problem::TempoClamped { id, milli_bpm, used });
        }
        self.engine.tempo_put(raw_tempo(id), tick, used)
    }

    // ---- tracks, clips, notes

    /// Adds the track with everything on it, behind `after_track`. Re-sends values when the
    /// track is already there.
    pub(crate) fn put_track(
        &mut self,
        project: &Project,
        id: TrackId,
        after_track: u64,
    ) -> Result<(), Error> {
        let Some(track) = project.track(id) else {
            return Ok(());
        };
        let raw = raw_track(id);
        let c = track.content.as_ref();
        self.engine.track_add(raw, after_track, &track.shell.name)?;
        self.engine.track_set_name(raw, &track.shell.name)?;
        self.engine.track_set_mix(raw, c.gain, c.pan, c.mute)?;
        let mut behind = 0;
        for device in &c.devices {
            if self.put_device(id, device, behind)? {
                behind = raw_device(device.id);
            }
        }
        for (clip_id, clip) in &c.clips {
            self.put_clip(id, *clip_id, clip)?;
        }
        Ok(())
    }

    pub(crate) fn put_clip(
        &mut self,
        track: TrackId,
        id: ClipId,
        clip: &mdaw_model::Clip,
    ) -> Result<(), Error> {
        let raw = raw_clip(id);
        if clip.length.0 < 1 {
            self.problems.push(Problem::ClipSkipped { track, clip: id });
            return self.engine.clip_remove(raw);
        }
        self.engine
            .clip_add(raw_track(track), raw, clip.start.0, clip.length.0, &clip.name)?;
        // No-ops when the clip was just added; set range and name when it was already there.
        self.engine.clip_set_range(raw, clip.start.0, clip.length.0)?;
        self.engine.clip_set_name(raw, &clip.name)?;
        self.put_notes(track, id, clip.notes.iter().map(|(id, note)| (*id, note)))
    }

    pub(crate) fn put_notes<'n>(
        &mut self,
        track: TrackId,
        clip: ClipId,
        notes: impl IntoIterator<Item = (NoteId, &'n Note)>,
    ) -> Result<(), Error> {
        let mut good = Vec::new();
        let mut bad = Vec::new();
        for (id, note) in notes {
            match engine_note(id, note) {
                Some(n) => good.push(n),
                None => {
                    self.problems.push(Problem::NoteSkipped {
                        track,
                        clip,
                        note: id,
                    });
                    bad.push(raw_note(id));
                }
            }
        }
        if !good.is_empty() {
            self.engine.notes_put(raw_clip(clip), &good)?;
        }
        if !bad.is_empty() {
            // A note set out of range no longer sounds; one that was in the engine goes.
            self.engine.notes_remove(raw_clip(clip), &bad)?;
        }
        Ok(())
    }

    // ---- devices

    /// The device in the engine that `device` goes behind: the nearest one to its left in the
    /// model that the engine has (a skipped plugin is not there), 0 if none.
    pub(crate) fn device_predecessor(&self, content: &TrackContent, device: DeviceId) -> u64 {
        let Some(index) = content.devices.iter().position(|d| d.id == device) else {
            return 0;
        };
        content
            .devices
            .iter()
            .take(index)
            .rev()
            .map(|d| raw_device(d.id))
            .find(|&raw| self.engine.has(EntityKind::Device, raw))
            .unwrap_or(0)
    }

    /// Adds a device with its bypass, parameters and state. Returns whether the engine has it.
    pub(crate) fn put_device(
        &mut self,
        track: TrackId,
        device: &Device,
        after_device: u64,
    ) -> Result<bool, Error> {
        let raw = raw_device(device.id);
        let added = match &device.kind {
            // The engine's own synth plays both built-in devices.
            DeviceKind::Builtin { .. } => self
                .engine
                .device_add(raw_track(track), raw, after_device, "builtin", "4osc")
                .map(|()| true)?,
            DeviceKind::Plugin { format, id, .. } => {
                let plugins = match &self.plugins {
                    Some(list) => list,
                    None => self.plugins.insert(self.engine.plugins()?),
                };
                let skipped = |reason: String| Problem::DeviceSkipped {
                    track,
                    device: device.id,
                    format: *format,
                    plugin: id.clone(),
                    reason,
                };
                match find_plugin(plugins, *format, id) {
                    None => {
                        self.problems
                            .push(skipped("not installed, or not scanned".to_string()));
                        false
                    }
                    Some(info) => {
                        let identifier = info.identifier.clone();
                        match self.engine.device_add(
                            raw_track(track),
                            raw,
                            after_device,
                            format.name(),
                            &identifier,
                        ) {
                            Ok(()) => true,
                            Err(e) if matches!(e.status, Status::NotFound | Status::Plugin) => {
                                self.problems.push(skipped(e.message));
                                false
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
            }
        };
        if !added {
            return Ok(false);
        }
        self.engine.device_set_bypass(raw, device.bypass)?;
        match &device.kind {
            DeviceKind::Builtin { .. } => self.put_builtin(track, device)?,
            DeviceKind::Plugin { state, .. } => {
                if let Some(blob) = state {
                    self.put_state(track, device.id, *blob);
                }
            }
        }
        Ok(true)
    }

    /// Sets the engine's synth up from all of a built-in device's parameters, defaults included.
    pub(crate) fn put_builtin(&mut self, track: TrackId, device: &Device) -> Result<(), Error> {
        match &device.kind {
            DeviceKind::Builtin {
                kind: BuiltinKind::TwoOsc,
                ..
            } => self.put_two_osc(track, device),
            DeviceKind::Plugin { .. } => Ok(()),
        }
    }

    /// Carries changes of some of a built-in device's parameters over; the values are read from
    /// `device`.
    pub(crate) fn put_builtin_changes(
        &mut self,
        track: TrackId,
        device: &Device,
        params: impl IntoIterator<Item = u32>,
    ) -> Result<(), Error> {
        let DeviceKind::Builtin {
            kind: BuiltinKind::TwoOsc,
            params: held,
        } = &device.kind
        else {
            return Ok(());
        };
        let raw = raw_device(device.id);
        let values = TwoOscValues::from_map(held);
        let mut state = false;
        for param in params {
            match two_osc_map::target(param) {
                None => self.problems.push(Problem::ParamNotMapped {
                    track,
                    device: device.id,
                    param,
                }),
                Some(Target::State) => state = true,
                Some(Target::Param {
                    four_osc,
                    range,
                    convert,
                }) => {
                    let plain = convert(values.get(param));
                    self.engine
                        .device_set_param(raw, four_osc, range.normalise(plain) as f32)?;
                }
            }
        }
        if state {
            self.put_two_osc_state(raw, &values)?;
        }
        Ok(())
    }

    /// Sets 4osc up as the built-in instrument (`BuiltinKind::TwoOsc`, see [`crate::two_osc`]):
    /// first the settings that are part of its state, then every parameter.
    fn put_two_osc(&mut self, track: TrackId, device: &Device) -> Result<(), Error> {
        let Some(held) = device.params() else {
            return Ok(());
        };
        let raw = raw_device(device.id);
        let values = TwoOscValues::from_map(held);
        self.put_two_osc_state(raw, &values)?;
        for p in &mdaw_model::two_osc::PARAMS {
            if let Some(Target::Param {
                four_osc,
                range,
                convert,
            }) = two_osc_map::target(p.id)
            {
                let plain = convert(values.get(p.id));
                self.engine
                    .device_set_param(raw, four_osc, range.normalise(plain) as f32)?;
            }
        }
        for (four_osc, range, plain) in two_osc_map::FIXED_PARAMS {
            self.engine
                .device_set_param(raw, four_osc, range.normalise(*plain) as f32)?;
        }
        for param in held.keys() {
            if two_osc_map::target(*param).is_none() {
                self.problems.push(Problem::ParamNotMapped {
                    track,
                    device: device.id,
                    param: *param,
                });
            }
        }
        Ok(())
    }

    /// Patches the instrument's settings into 4osc's state; no call if it holds them already.
    fn put_two_osc_state(&mut self, raw: u64, values: &TwoOscValues) -> Result<(), Error> {
        let state = self.engine.device_get_state(raw);
        let patched = value_tree::with_int_properties(&state, &two_osc_map::state_properties(values))
            .ok_or_else(|| {
                Error::new(
                    Status::Internal,
                    "the engine's synth gave a state that is not a ValueTree",
                )
            })?;
        if patched != state {
            self.engine.device_set_state(raw, &patched)?;
        }
        Ok(())
    }

    /// A state the plugin refuses is a problem, not an error: the plugin is in the engine either
    /// way.
    pub(crate) fn put_state(&mut self, track: TrackId, device: DeviceId, blob: BlobHash) {
        let not_applied = |reason: String| Problem::StateNotApplied {
            track,
            device,
            blob,
            reason,
        };
        match (self.blobs)(&blob) {
            None => self.problems.push(Problem::StateMissing { track, device, blob }),
            Some(bytes) => {
                if let Err(e) = self.engine.device_set_state(raw_device(device), &bytes) {
                    self.problems.push(not_applied(e.message));
                }
            }
        }
    }
}

// ---- the engine's synth ------------------------------------------------------------------------

/// A 4osc parameter's range, as Tracktion's `FourOscPlugin` declares it
/// (`juce::NormalisableRange (start, end, interval, skew)`): normalised = proportion^skew.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FourOscRange {
    start: f64,
    end: f64,
    skew: f64,
}

impl FourOscRange {
    pub(crate) fn normalise(self, plain: f64) -> f64 {
        let proportion = ((plain - self.start) / (self.end - self.start)).clamp(0.0, 1.0);
        if self.skew == 1.0 {
            proportion
        } else {
            proportion.powf(self.skew)
        }
    }
}

/// The ranges of the 4osc parameters the built-in instrument uses.
pub(crate) mod four_osc_range {
    use super::FourOscRange;

    /// amp attack, decay, release: 0.001..60 s, skewed.
    pub(crate) const AMP_TIME: FourOscRange = FourOscRange {
        start: 0.001,
        end: 60.0,
        skew: 0.2,
    };
    /// filter attack, decay, release: 0..60 s, skewed.
    pub(crate) const FILTER_TIME: FourOscRange = FourOscRange {
        start: 0.0,
        end: 60.0,
        skew: 0.2,
    };
    /// Sustain levels, resonance, key tracking: 0..100 %.
    pub(crate) const PERCENT: FourOscRange = FourOscRange {
        start: 0.0,
        end: 100.0,
        skew: 1.0,
    };
    /// Oscillator and master levels: -100..0 dB, skewed (-100 dB is silence).
    pub(crate) const LEVEL_DB: FourOscRange = FourOscRange {
        start: -100.0,
        end: 0.0,
        skew: 4.0,
    };
    /// Oscillator tune: -36..36 semitones in steps of 1.
    pub(crate) const TUNE: FourOscRange = FourOscRange {
        start: -36.0,
        end: 36.0,
        skew: 1.0,
    };
    /// Oscillator fine tune: -100..100 cents.
    pub(crate) const FINE_TUNE: FourOscRange = FourOscRange {
        start: -100.0,
        end: 100.0,
        skew: 1.0,
    };
    /// Pulse width: 0.01..0.99.
    pub(crate) const PULSE_WIDTH: FourOscRange = FourOscRange {
        start: 0.01,
        end: 0.99,
        skew: 1.0,
    };
    /// Filter frequency as a MIDI note number: 0..135.076232 (20 kHz).
    pub(crate) const FILTER_NOTE: FourOscRange = FourOscRange {
        start: 0.0,
        end: 135.076_232,
        skew: 1.0,
    };
    /// Filter envelope amount: -1..1, in units of 137 semitones.
    pub(crate) const FILTER_AMOUNT: FourOscRange = FourOscRange {
        start: -1.0,
        end: 1.0,
        skew: 1.0,
    };
    /// Legato (glide) time: 0..500 ms.
    pub(crate) const GLIDE: FourOscRange = FourOscRange {
        start: 0.0,
        end: 500.0,
        skew: 1.0,
    };
}

/// Just enough of JUCE's binary `ValueTree` format (`ValueTree::writeToStream`, which is what
/// the engine gives as a built-in device's state) to set integer properties of the top-level
/// tree; everything else is copied as it is.
mod value_tree {
    fn read_string<'a>(data: &mut &'a [u8]) -> Option<&'a [u8]> {
        let end = data.iter().position(|&b| b == 0)?;
        let (text, rest) = data.split_at(end);
        *data = &rest[1..];
        Some(text)
    }

    /// `OutputStream::writeCompressedInt`: a byte with the count of little-endian bytes that
    /// follow (and 0x80 for negative), then those bytes.
    fn read_compressed_int(data: &mut &[u8]) -> Option<i32> {
        let (&size, rest) = data.split_first()?;
        let count = usize::from(size & 0x7f);
        if count > 4 || rest.len() < count {
            return None;
        }
        let mut bytes = [0u8; 4];
        bytes[..count].copy_from_slice(&rest[..count]);
        *data = &rest[count..];
        let value = i32::from_le_bytes(bytes);
        Some(if size & 0x80 != 0 { -value } else { value })
    }

    fn write_compressed_int(out: &mut Vec<u8>, value: i32) {
        let magnitude = value.unsigned_abs().to_le_bytes();
        let count = magnitude.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        out.push(count as u8 | if value < 0 { 0x80 } else { 0 });
        out.extend_from_slice(&magnitude[..count]);
    }

    /// An int `var` as `var::writeToStream` writes it: size 5, marker 1, 4 bytes little-endian.
    fn int_var(value: i32) -> Vec<u8> {
        let mut out = Vec::with_capacity(6);
        write_compressed_int(&mut out, 5);
        out.push(1);
        out.extend_from_slice(&value.to_le_bytes());
        out
    }

    /// The tree with each named property set to its int value (added if missing). `None` if
    /// `tree` is not a ValueTree.
    pub(super) fn with_int_properties(tree: &[u8], set: &[(&str, i32)]) -> Option<Vec<u8>> {
        let mut data = tree;
        let kind = read_string(&mut data)?;
        if kind.is_empty() {
            return None;
        }
        let count = read_compressed_int(&mut data)?;
        let mut properties: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for _ in 0..count {
            let name = read_string(&mut data)?.to_vec();
            let size = usize::try_from(read_compressed_int(&mut data)?).ok()?;
            if data.len() < size {
                return None;
            }
            let (value, rest) = data.split_at(size);
            let mut stored = Vec::new();
            write_compressed_int(&mut stored, size as i32);
            stored.extend_from_slice(value);
            properties.push((name, stored));
            data = rest;
        }
        for (name, value) in set {
            let value = int_var(*value);
            match properties.iter_mut().find(|(n, _)| n == name.as_bytes()) {
                Some((_, stored)) => *stored = value,
                None => properties.push((name.as_bytes().to_vec(), value)),
            }
        }
        let mut out = Vec::with_capacity(tree.len() + 16 * set.len());
        out.extend_from_slice(kind);
        out.push(0);
        write_compressed_int(&mut out, properties.len() as i32);
        for (name, value) in properties {
            out.extend_from_slice(&name);
            out.push(0);
            out.extend_from_slice(&value);
        }
        // The children, untouched.
        out.extend_from_slice(data);
        Some(out)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn properties_are_replaced_and_added_children_kept() {
            // PLUGIN { type = "4osc", waveShape1 = 1 } with one child "MODMATRIX" {}.
            let mut tree = b"PLUGIN\0\x01\x02type\0".to_vec();
            tree.extend_from_slice(&[0x01, 0x06, 0x05]);
            tree.extend_from_slice(b"4osc\0");
            tree.extend_from_slice(b"waveShape1\0");
            tree.extend_from_slice(&int_var(1));
            tree.extend_from_slice(&[0x01, 0x01]);
            tree.extend_from_slice(b"MODMATRIX\0\x00\x00");

            let out = with_int_properties(&tree, &[("waveShape1", 3), ("filterType", 1)]).unwrap();
            let mut expected = b"PLUGIN\0\x01\x03type\0".to_vec();
            expected.extend_from_slice(&[0x01, 0x06, 0x05]);
            expected.extend_from_slice(b"4osc\0");
            expected.extend_from_slice(b"waveShape1\0");
            expected.extend_from_slice(&[0x01, 0x05, 0x01, 3, 0, 0, 0]);
            expected.extend_from_slice(b"filterType\0");
            expected.extend_from_slice(&[0x01, 0x05, 0x01, 1, 0, 0, 0]);
            expected.extend_from_slice(&[0x01, 0x01]);
            expected.extend_from_slice(b"MODMATRIX\0\x00\x00");
            assert_eq!(out, expected);
            // Setting what is there already changes nothing.
            assert_eq!(with_int_properties(&out, &[("filterType", 1)]).unwrap(), out);
        }

        #[test]
        fn compressed_ints_round_trip() {
            for value in [0, 1, 5, 255, 256, 70_000, -3, i32::MAX] {
                let mut out = Vec::new();
                write_compressed_int(&mut out, value);
                assert_eq!(read_compressed_int(&mut out.as_slice()), Some(value));
            }
            assert!(with_int_properties(b"", &[]).is_none());
            assert!(with_int_properties(b"PLUGIN\0\x01\x01x\0\x01\x09", &[]).is_none());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdaw_model::{ActorId, IdGen, TempoPoint, Tick};

    #[test]
    fn plugins_are_found_by_format_and_own_id() {
        let info = |format: &str, identifier: &str| PluginInfo {
            format: format.into(),
            identifier: identifier.into(),
            name: "mdaw Test Synth".into(),
            vendor: String::new(),
            instrument: true,
        };
        let list = vec![
            info("LV2", "org.mdaw.test-synth"),
            info("CLAP", "org.mdaw.test-synth"),
        ];
        let found = find_plugin(&list, PluginFormat::Clap, "org.mdaw.test-synth");
        assert_eq!(found.map(|p| p.format.as_str()), Some("CLAP"));
        assert!(find_plugin(&list, PluginFormat::Clap, "org.mdaw.test").is_none());
        assert!(find_plugin(&list, PluginFormat::Clap, "org.mdaw.test-synth.x").is_none());
    }

    #[test]
    fn four_osc_ranges_normalise() {
        use four_osc_range::{AMP_TIME, LEVEL_DB};
        assert!((AMP_TIME.normalise(60.0) - 1.0).abs() < 1e-12 && AMP_TIME.normalise(0.001) == 0.0);
        assert!((LEVEL_DB.normalise(-50.0) - 0.5f64.powi(4)).abs() < 1e-12);
    }

    #[test]
    fn effective_tempo_keeps_the_highest_id_per_tick() {
        let mut ids = IdGen::new(ActorId(1));
        let (a, b, c) = (ids.tempo(), ids.tempo(), ids.tempo());
        let mut project = Project::default();
        let point = |tick, bpm| TempoPoint {
            tick: Tick(tick),
            milli_bpm: bpm,
        };
        project.tempo.insert(b, point(0, 100_000));
        project.tempo.insert(a, point(-5, 90_000));
        project.tempo.insert(c, point(960, 80_000));
        let eff = effective_tempo(&project);
        assert_eq!(eff.get(&0), Some(&(b, 100_000)));
        assert_eq!(eff.get(&960), Some(&(c, 80_000)));
        assert_eq!(eff.len(), 2);
    }
}
