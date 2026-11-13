//! The engine's project, derived from the canonical model (ADR 0001).
//!
//! [`load_project`] clears the engine and builds everything from a project. [`apply_op`] carries
//! one model op over without a rebuild. Both read the values to send from the model state
//! *after* the op, so applying an op twice, or an op the model ignored, leaves the engine as the
//! model is. The engine addresses everything by the model's own ids.
//!
//! What the engine cannot take does not fail the call: it is left out and reported as a
//! [`Problem`]. Model data the engine has no use for (project name, track colour,
//! stems) is ignored without a report.
//!
//! A plugin device's settings are its state blob and its parameter values; the state is applied
//! first, then the values (which the model keeps in the plugin's own units, converted here with
//! the range the engine reports).
//!
//! The built-in Pocket synth is played by the engine's own synth, Tracktion's 4osc, set up from
//! Pocket's parameters through [`POCKET_TO_FOUR_OSC`]. That is an approximation for the
//! migration, not the same synth: the oscillators, filter and envelopes are 4osc's, and only
//! their settings follow Pocket's.

use crate::{Engine, EntityKind, Error, Note as EngineNote, ParamInfo, PluginInfo, Status};
use mdaw_dsp::pocket::{PocketParams, param as pocket};
use mdaw_model::{
    BlobHash, BuiltinKind, ClipId, ClipProp, Device, DeviceId, DeviceKind, MAX_MILLI_BPM, MAX_TICK,
    MIN_MILLI_BPM, Note, NoteId, Op, PluginFormat, Project, ShellProp, TempoId, TrackContent, TrackId,
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
    /// A device parameter the engine cannot be given: one the plugin does not have, or one the
    /// built-in Pocket synth does not have (the engine's stand-in for Pocket takes all of
    /// Pocket's own, see [`POCKET_TO_FOUR_OSC`]).
    ParamNotMapped {
        track: TrackId,
        device: DeviceId,
        param: u32,
    },
    /// A plugin's saved state was not restored: the blob is not at hand, or the plugin refused
    /// it.
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

/// Carries one model op over to the engine, incrementally.
///
/// `before` and `after` are the model states on either side of `mdaw_model::apply(op)` (equal
/// if the model ignored the op). The values sent are read from `after`; `before` is needed only
/// for where a tempo point was. `blobs` gives plugin states by hash, for `DeviceSetState` and for
/// devices inside `DeviceAdd` / `TrackAdd`.
pub fn apply_op(
    engine: &Engine,
    before: &Project,
    op: &Op,
    after: &Project,
    blobs: BlobResolver<'_>,
) -> Result<Vec<Problem>, Error> {
    let mut sync = Sync::new(engine, blobs);
    sync.op(before, op, after)?;
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

/// The track to put `track` behind in the engine: its left neighbour in `project`, 0 if first.
fn track_predecessor(project: &Project, track: TrackId) -> u64 {
    let order = ordered_tracks(project);
    match order.iter().position(|(id, _)| *id == track) {
        Some(index) if index > 0 => raw_track(order[index - 1].0),
        _ => 0,
    }
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

    pub(crate) fn op(&mut self, before: &Project, op: &Op, after: &Project) -> Result<(), Error> {
        let engine = self.engine;
        match op {
            // Project scope.
            Op::ProjectSetName { .. } => {}
            Op::MasterSetGain { .. } => engine.master_set_gain(after.master_gain)?,
            Op::TempoPut { id, .. } | Op::TempoRemove { id } => self.tempo_changed(before, *id, after)?,
            Op::TrackAdd { id, .. } => {
                if after.track(*id).is_some() {
                    self.put_track(after, *id, track_predecessor(after, *id))?;
                }
            }
            Op::TrackMove { id, .. } => {
                if after.track(*id).is_some() {
                    engine.track_move(raw_track(*id), track_predecessor(after, *id))?;
                }
            }
            Op::TrackSetShell { id, prop } => {
                // Colour: the engine has no use for it.
                if let (ShellProp::Name(_), Some(track)) = (prop, after.track(*id)) {
                    engine.track_set_name(raw_track(*id), &track.shell.name)?;
                }
            }
            Op::TrackRemove { id } => {
                if after.track(*id).is_none() {
                    engine.track_remove(raw_track(*id))?;
                }
            }

            // Track content.
            Op::TrackSetMix { track, .. } => {
                if let Some(c) = content(after, *track) {
                    engine.track_set_mix(raw_track(*track), c.gain, c.pan, c.mute)?;
                }
            }
            Op::ClipAdd { track, id, .. } => {
                if let Some(clip) = content(after, *track).and_then(|c| c.clips.get(id)) {
                    self.put_clip(*track, *id, clip)?;
                }
            }
            Op::ClipRemove { track, id } => {
                if content(after, *track).is_none_or(|c| !c.clips.contains_key(id)) {
                    engine.clip_remove(raw_clip(*id))?;
                }
            }
            Op::ClipSet { track, id, prop } => {
                let Some(clip) = content(after, *track).and_then(|c| c.clips.get(id)) else {
                    return Ok(());
                };
                match prop {
                    ClipProp::Start(_) | ClipProp::Length(_) => {
                        let was_in_engine = content(before, *track)
                            .and_then(|c| c.clips.get(id))
                            .is_some_and(|c| c.length.0 >= 1);
                        if was_in_engine && clip.length.0 >= 1 {
                            engine.clip_set_range(raw_clip(*id), clip.start.0, clip.length.0)?;
                        } else {
                            // Into or out of the engine's range: add (with its notes) or remove.
                            self.put_clip(*track, *id, clip)?;
                        }
                    }
                    ClipProp::Name(_) => engine.clip_set_name(raw_clip(*id), &clip.name)?,
                }
            }
            Op::NoteAdd { track, clip, id, .. } | Op::NoteSet { track, clip, id, .. } => {
                let found = content(after, *track).and_then(|c| c.clips.get(clip));
                if let Some(note) = found.filter(|c| c.length.0 >= 1).and_then(|c| c.notes.get(id)) {
                    self.put_notes(*track, *clip, [(*id, note)])?;
                }
            }
            Op::NoteRemove { track, clip, id } => {
                let still_there = content(after, *track)
                    .and_then(|c| c.clips.get(clip))
                    .is_some_and(|c| c.notes.contains_key(id));
                if !still_there {
                    engine.notes_remove(raw_clip(*clip), &[raw_note(*id)])?;
                }
            }
            Op::DeviceAdd { track, device, .. } => {
                if let Some(device) = content(after, *track).and_then(|c| c.device(device.id)) {
                    let behind = self.device_predecessor(after, *track, device.id);
                    self.put_device(*track, device, behind)?;
                }
            }
            Op::DeviceRemove { track, id } => {
                if content(after, *track).is_none_or(|c| c.device(*id).is_none()) {
                    engine.device_remove(raw_device(*id))?;
                }
            }
            Op::DeviceMove { track, id, .. } => {
                if content(after, *track).is_some_and(|c| c.device(*id).is_some()) {
                    let behind = self.device_predecessor(after, *track, *id);
                    engine.device_move(raw_device(*id), behind)?;
                }
            }
            Op::DeviceSetParam { track, id, param, .. } => {
                let Some(device) = content(after, *track).and_then(|c| c.device(*id)) else {
                    return Ok(());
                };
                match &device.kind {
                    DeviceKind::Builtin { .. } => {
                        if engine.has(EntityKind::Device, raw_device(*id)) {
                            self.put_pocket(*track, device)?;
                        }
                    }
                    DeviceKind::Plugin { .. } => {
                        if engine.has(EntityKind::Device, raw_device(*id)) {
                            let params = engine.device_params(raw_device(*id))?;
                            self.put_param(*track, *id, &params, *param, device.params.get(param).copied())?;
                        }
                    }
                }
            }
            Op::DeviceSetBypass { track, id, .. } => {
                if let Some(device) = content(after, *track).and_then(|c| c.device(*id)) {
                    engine.device_set_bypass(raw_device(*id), device.bypass)?;
                }
            }
            Op::DeviceSetState { track, id, .. } => {
                let Some(device) = content(after, *track).and_then(|c| c.device(*id)) else {
                    return Ok(());
                };
                let DeviceKind::Plugin { state, .. } = &device.kind else {
                    // The model ignores state for built-in devices.
                    return Ok(());
                };
                if !engine.has(EntityKind::Device, raw_device(*id)) {
                    return Ok(());
                }
                match state {
                    Some(blob) => self.put_state(*track, *id, *blob),
                    // No saved state: the plugin as it starts.
                    None => engine.device_reset_state(raw_device(*id))?,
                }
                // The parameter values go on top of the state, as when the project is loaded.
                self.put_params(*track, device)?;
            }
        }
        Ok(())
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

    /// A tempo point was put or removed. The engine holds the winner of each tick; the ticks
    /// that can have changed are where the point was and where it is now.
    fn tempo_changed(&mut self, before: &Project, id: TempoId, after: &Project) -> Result<(), Error> {
        let winners = effective_tempo(after);
        let now = after.tempo.get(&id).map(|p| tempo_tick(p.tick.0));
        let was = before.tempo.get(&id).map(|p| tempo_tick(p.tick.0));
        let wins = now.is_some_and(|tick| winners.get(&tick).is_some_and(|(w, _)| *w == id));
        if !wins {
            self.engine.tempo_remove(raw_tempo(id))?;
        }
        let mut ticks: Vec<i64> = was.into_iter().chain(now).collect();
        ticks.dedup();
        for tick in ticks {
            if let Some(&(winner, milli_bpm)) = winners.get(&tick) {
                self.put_tempo(winner, tick, milli_bpm)?;
            }
        }
        Ok(())
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
    pub(crate) fn device_predecessor(&self, project: &Project, track: TrackId, device: DeviceId) -> u64 {
        let Some(c) = content(project, track) else {
            return 0;
        };
        let Some(index) = c.devices.iter().position(|d| d.id == device) else {
            return 0;
        };
        c.devices
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
            // The engine's own synth stands in for Pocket until the engine has Pocket.
            DeviceKind::Builtin {
                kind: BuiltinKind::Pocket,
                ..
            } => self
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
            DeviceKind::Builtin { .. } => self.put_pocket(track, device)?,
            DeviceKind::Plugin { state, .. } => {
                if let Some(blob) = state {
                    self.put_state(track, device.id, *blob);
                }
                self.put_params(track, device)?;
            }
        }
        Ok(true)
    }

    /// Gives a plugin device the parameter values the model holds for it.
    pub(crate) fn put_params(&mut self, track: TrackId, device: &Device) -> Result<(), Error> {
        if device.params.is_empty() {
            return Ok(());
        }
        let params = match &device.kind {
            DeviceKind::Plugin { .. } => self.engine.device_params(raw_device(device.id))?,
            DeviceKind::Builtin { .. } => return self.put_pocket(track, device),
        };
        for (param, value) in &device.params {
            self.put_param(track, device.id, &params, *param, Some(*value))?;
        }
        Ok(())
    }

    /// Sets the engine's synth up from all of a Pocket device's parameters, defaults included
    /// (see [`POCKET_TO_FOUR_OSC`]): first the settings that are part of 4osc's state, then its
    /// parameters. Parameter ids Pocket does not have are reported.
    pub(crate) fn put_pocket(&mut self, track: TrackId, device: &Device) -> Result<(), Error> {
        let raw = raw_device(device.id);
        let values = PocketParams::from_map(&device.params);
        let state = self.engine.device_get_state(raw);
        let properties: Vec<(&str, i32)> = FOUR_OSC_FIXED_STATE
            .iter()
            .copied()
            .chain(
                POCKET_TO_FOUR_OSC_STATE
                    .iter()
                    .map(|(from, property, convert)| (*property, convert(values.0[*from]))),
            )
            .collect();
        let patched = value_tree::with_int_properties(&state, &properties).ok_or_else(|| {
            Error::new(
                Status::Internal,
                "the engine's synth gave a state that is not a ValueTree",
            )
        })?;
        if patched != state {
            self.engine.device_set_state(raw, &patched)?;
        }
        for row in POCKET_TO_FOUR_OSC {
            let plain = row.convert.apply(values.0[row.pocket]);
            self.engine
                .device_set_param(raw, row.four_osc, row.range.normalise(plain) as f32)?;
        }
        for param in device.params.keys() {
            if *param as usize >= pocket::COUNT {
                self.problems.push(Problem::ParamNotMapped {
                    track,
                    device: device.id,
                    param: *param,
                });
            }
        }
        Ok(())
    }

    /// Sets one parameter of a plugin device. `plain` is the model's value, in the plugin's own
    /// units; `None` is the parameter's default. `params` is the device's list from the engine.
    pub(crate) fn put_param(
        &mut self,
        track: TrackId,
        device: DeviceId,
        params: &[ParamInfo],
        param: u32,
        plain: Option<f64>,
    ) -> Result<(), Error> {
        let id = param.to_string();
        let found = params.iter().find(|p| p.id == id);
        let value = found.and_then(|p| match plain {
            Some(plain) => Some(p.normalise(plain)),
            None => p.default,
        });
        let Some(value) = value else {
            self.problems
                .push(Problem::ParamNotMapped { track, device, param });
            return Ok(());
        };
        self.engine
            .device_set_param(raw_device(device), &id, value as f32)
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
            None => self
                .problems
                .push(not_applied("the blob is not at hand".to_string())),
            Some(bytes) => {
                if let Err(e) = self.engine.device_set_state(raw_device(device), &bytes) {
                    self.problems.push(not_applied(e.message));
                }
            }
        }
    }
}

// ---- Pocket on the engine's synth ------------------------------------------------------------------

/// A 4osc parameter's range, as Tracktion's `FourOscPlugin` declares it
/// (`juce::NormalisableRange (start, end, interval, skew)`): normalised = proportion^skew.
#[derive(Clone, Copy, Debug)]
struct FourOscRange {
    start: f64,
    end: f64,
    skew: f64,
}

impl FourOscRange {
    fn normalise(self, plain: f64) -> f64 {
        let proportion = ((plain - self.start) / (self.end - self.start)).clamp(0.0, 1.0);
        if self.skew == 1.0 {
            proportion
        } else {
            proportion.powf(self.skew)
        }
    }
}

/// amp attack, decay, release: 0.001..60 s, skewed.
const AMP_TIME: FourOscRange = FourOscRange {
    start: 0.001,
    end: 60.0,
    skew: 0.2,
};
/// filter attack, decay, release: 0..60 s, skewed.
const FILTER_TIME: FourOscRange = FourOscRange {
    start: 0.0,
    end: 60.0,
    skew: 0.2,
};
/// Sustain levels, resonance, key tracking: 0..100 %.
const PERCENT: FourOscRange = FourOscRange {
    start: 0.0,
    end: 100.0,
    skew: 1.0,
};
/// Oscillator and master levels: -100..0 dB, skewed (-100 dB is silence).
const LEVEL_DB: FourOscRange = FourOscRange {
    start: -100.0,
    end: 0.0,
    skew: 4.0,
};
/// Oscillator tune: -36..36 semitones in steps of 1.
const TUNE: FourOscRange = FourOscRange {
    start: -36.0,
    end: 36.0,
    skew: 1.0,
};
/// Oscillator fine tune: -100..100 cents.
const FINE_TUNE: FourOscRange = FourOscRange {
    start: -100.0,
    end: 100.0,
    skew: 1.0,
};
/// Pulse width: 0.01..0.99.
const PULSE_WIDTH: FourOscRange = FourOscRange {
    start: 0.01,
    end: 0.99,
    skew: 1.0,
};
/// Filter frequency as a MIDI note number: 0..135.076232 (20 kHz).
const FILTER_NOTE: FourOscRange = FourOscRange {
    start: 0.0,
    end: 135.076_232,
    skew: 1.0,
};
/// Filter envelope amount: -1..1, in units of 137 semitones.
const FILTER_AMOUNT: FourOscRange = FourOscRange {
    start: -1.0,
    end: 1.0,
    skew: 1.0,
};

/// How a Pocket value (in Pocket's units) becomes a 4osc value (in 4osc's units).
#[derive(Clone, Copy, Debug)]
enum Convert {
    Same,
    /// 0..1 to percent.
    Percent,
    /// A linear gain to dB (0 to -100 dB, silence).
    Decibels,
    /// `1 - x` as a gain, to dB: the first oscillator's share of Pocket's oscillator mix.
    ComplementDecibels,
    /// Hz to a MIDI note number.
    MidiNote,
    /// Pocket's resonance (state-variable filter, Q = 1 / (2 - 1.9 r)) to 4osc's (biquad,
    /// Q = 0.7071 / (1 - 0.99 res/100)) with the same Q; 4osc's Q cannot go below 0.7071.
    Resonance,
    /// Octaves of filter envelope to 4osc's units of 137 semitones.
    FilterAmount,
}

impl Convert {
    fn apply(self, value: f32) -> f64 {
        let v = f64::from(value);
        let decibels = |gain: f64| {
            if gain > 1e-5 {
                (20.0 * gain.log10()).max(-100.0)
            } else {
                -100.0
            }
        };
        match self {
            Convert::Same => v,
            Convert::Percent => v * 100.0,
            Convert::Decibels => decibels(v),
            Convert::ComplementDecibels => decibels(1.0 - v),
            Convert::MidiNote => 12.0 * (v.max(1.0) / 440.0).log2() + 69.0,
            Convert::Resonance => {
                let q = 1.0 / (2.0 - 1.9 * v);
                (100.0 * (1.0 - std::f64::consts::FRAC_1_SQRT_2 / q) / 0.99).max(0.0)
            }
            Convert::FilterAmount => v * 12.0 / 137.0,
        }
    }
}

/// One 4osc parameter set from one Pocket parameter.
#[derive(Clone, Copy, Debug)]
struct PocketRow {
    pocket: usize,
    four_osc: &'static str,
    range: FourOscRange,
    convert: Convert,
}

const fn row(pocket: usize, four_osc: &'static str, range: FourOscRange, convert: Convert) -> PocketRow {
    PocketRow {
        pocket,
        four_osc,
        range,
        convert,
    }
}

/// Pocket's parameters (`mdaw_dsp::pocket::PARAMS`) on Tracktion's 4osc, which plays Pocket
/// devices in the engine. Every Pocket parameter has a counterpart:
///
/// | Pocket                   | 4osc                                  | conversion                      |
/// |--------------------------|---------------------------------------|---------------------------------|
/// | 0 Osc 1 (Saw/Pulse/Sine) | state `waveShape1`                    | saw / square / sine             |
/// | 1 Osc 2                  | state `waveShape2`                    | saw / square / sine             |
/// | 2 Osc 2 semi             | `tune2`                               | semitones                       |
/// | 3 Osc 2 fine             | `fineTune2`                           | cents                           |
/// | 4 Osc mix                | `level1`, `level2`                    | gains 1 - mix and mix, in dB    |
/// | 5 Pulse width            | `pulseWidth1`, `pulseWidth2`          | same                            |
/// | 6 Noise                  | state `waveShape3` (noise), `level3`  | gain in dB                      |
/// | 7, 8 Attack, Decay       | `ampAttack`, `ampDecay`               | seconds                         |
/// | 9 Sustain                | `ampSustain`                          | percent                         |
/// | 10 Release               | `ampRelease`                          | seconds                         |
/// | 11 Cutoff                | `filterFreq` (state `filterType` low-pass, `filterSlope` 12) | Hz to MIDI note |
/// | 12 Resonance             | `filterResonance`                     | same Q (4osc's lowest is 0.707) |
/// | 13 Filter env            | `filterAmount`                        | octaves to 137-semitone units   |
/// | 14..17 F attack..release | `filterAttack` .. `filterRelease`     | seconds, sustain in percent     |
/// | 18 Key track             | `filterKey`                           | percent                         |
/// | 19 Volume                | `masterLevel`                         | gain in dB                      |
///
/// Not the same synth: 4osc's oscillators, its filter (a biquad, not Pocket's state-variable
/// filter), its envelopes (Pocket's time is to -60 dB, 4osc's to silence), its velocity curve
/// and its voice count (32, Pocket 16) are its own. Wave shape, noise and filter type are not
/// parameters in 4osc but properties of its state, so they are set through the device's state
/// ([`POCKET_TO_FOUR_OSC_STATE`]). The ranges are those of Tracktion's FourOscPlugin.cpp; the
/// integration test reads the texts 4osc shows for the converted values to check them.
const POCKET_TO_FOUR_OSC: &[PocketRow] = &[
    row(pocket::OSC2_SEMI, "tune2", TUNE, Convert::Same),
    row(pocket::OSC2_FINE, "fineTune2", FINE_TUNE, Convert::Same),
    row(pocket::OSC_MIX, "level1", LEVEL_DB, Convert::ComplementDecibels),
    row(pocket::OSC_MIX, "level2", LEVEL_DB, Convert::Decibels),
    row(pocket::PULSE_WIDTH, "pulseWidth1", PULSE_WIDTH, Convert::Same),
    row(pocket::PULSE_WIDTH, "pulseWidth2", PULSE_WIDTH, Convert::Same),
    row(pocket::NOISE, "level3", LEVEL_DB, Convert::Decibels),
    row(pocket::AMP_ATTACK, "ampAttack", AMP_TIME, Convert::Same),
    row(pocket::AMP_DECAY, "ampDecay", AMP_TIME, Convert::Same),
    row(pocket::AMP_SUSTAIN, "ampSustain", PERCENT, Convert::Percent),
    row(pocket::AMP_RELEASE, "ampRelease", AMP_TIME, Convert::Same),
    row(pocket::CUTOFF, "filterFreq", FILTER_NOTE, Convert::MidiNote),
    row(pocket::RESONANCE, "filterResonance", PERCENT, Convert::Resonance),
    row(
        pocket::FILTER_ENV,
        "filterAmount",
        FILTER_AMOUNT,
        Convert::FilterAmount,
    ),
    row(pocket::FILTER_ATTACK, "filterAttack", FILTER_TIME, Convert::Same),
    row(pocket::FILTER_DECAY, "filterDecay", FILTER_TIME, Convert::Same),
    row(pocket::FILTER_SUSTAIN, "filterSustain", PERCENT, Convert::Percent),
    row(
        pocket::FILTER_RELEASE,
        "filterRelease",
        FILTER_TIME,
        Convert::Same,
    ),
    row(pocket::KEY_TRACK, "filterKey", PERCENT, Convert::Percent),
    row(pocket::VOLUME, "masterLevel", LEVEL_DB, Convert::Decibels),
];

/// 4osc's oscillator waves (`tracktion::Oscillator::Waves`): none 0, sine 1, square 2, saw 3,
/// triangle 4, noise 5. Pocket's: Saw 0, Pulse 1, Sine 2.
fn four_osc_wave(pocket_wave: f32) -> i32 {
    match pocket_wave as i32 {
        0 => 3,
        1 => 2,
        _ => 1,
    }
}

fn four_osc_noise(noise: f32) -> i32 {
    if noise > 0.0 { 5 } else { 0 }
}

/// Pocket parameters that are properties of 4osc's state, not parameters.
type StateRow = (usize, &'static str, fn(f32) -> i32);
const POCKET_TO_FOUR_OSC_STATE: &[StateRow] = &[
    (pocket::OSC1_WAVE, "waveShape1", four_osc_wave),
    (pocket::OSC2_WAVE, "waveShape2", four_osc_wave),
    (pocket::NOISE, "waveShape3", four_osc_noise),
];

/// Pocket's filter is always a 12 dB/octave low-pass; 4osc's starts switched off.
const FOUR_OSC_FIXED_STATE: &[(&str, i32)] = &[("filterType", 1), ("filterSlope", 12)];

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
    fn pocket_defaults_on_four_osc() {
        let defaults = PocketParams::default();
        let plain = |id: &str| {
            let row = POCKET_TO_FOUR_OSC.iter().find(|r| r.four_osc == id).unwrap();
            row.convert.apply(defaults.0[row.pocket])
        };
        assert!((plain("filterFreq") - (12.0 * (2500.0f64 / 440.0).log2() + 69.0)).abs() < 1e-9);
        assert!((plain("level1") + 6.0206).abs() < 1e-3 && (plain("level2") + 6.0206).abs() < 1e-3);
        assert!((plain("ampSustain") - 70.0).abs() < 1e-4);
        assert!((plain("masterLevel") - 20.0 * 0.7f64.log10()).abs() < 1e-4);
        assert_eq!(
            plain("filterResonance"),
            0.0,
            "Pocket's default Q is below 4osc's lowest"
        );
        assert!((Convert::Resonance.apply(1.0) - 93.9).abs() < 0.1);
        assert!((AMP_TIME.normalise(60.0) - 1.0).abs() < 1e-12 && AMP_TIME.normalise(0.001) == 0.0);
        assert!((LEVEL_DB.normalise(-50.0) - 0.5f64.powi(4)).abs() < 1e-12);
        assert_eq!(four_osc_wave(0.0), 3);
        assert_eq!(four_osc_wave(1.0), 2);
        assert_eq!(four_osc_wave(2.0), 1);
        // Every Pocket parameter is set from the table or the state rows.
        for id in 0..pocket::COUNT {
            assert!(
                POCKET_TO_FOUR_OSC.iter().any(|r| r.pocket == id)
                    || POCKET_TO_FOUR_OSC_STATE.iter().any(|r| r.0 == id),
                "Pocket parameter {id} has no counterpart"
            );
        }
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
