//! The engine's project, derived from the canonical model (ADR 0001).
//!
//! [`load_project`] clears the engine and builds everything from a project. [`apply_op`] carries
//! one model op over without a rebuild. Both read the values to send from the model state
//! *after* the op, so applying an op twice, or an op the model ignored, leaves the engine as the
//! model is. The engine addresses everything by the model's own ids.
//!
//! What the engine cannot take does not fail the call: it is left out and reported as a
//! [`Problem`]. Model data the engine has no use for (project name, track colour, clip name,
//! checkouts, stems) is ignored without a report.

use crate::{Engine, Error, Note as EngineNote, PluginInfo, Status};
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
    /// A device parameter the engine cannot be given. The built-in Pocket synth is played by
    /// the engine's own synth, which has other parameters; a plugin's model value is in the
    /// plugin's own units while the engine takes 0..1 and reports no range to convert with.
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
    /// A tempo outside the engine's range (20..300 bpm); `used` was sent instead.
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

/// Finds the engine's identifier for a model plugin. For CLAP the engine reports JUCE's
/// identifier string, which carries a 32-bit hash of the CLAP id at its end, not the id itself:
/// the match is on that hash (a collision would pick the wrong plugin; nothing better is
/// possible through the current interface).
pub fn find_plugin<'a>(plugins: &'a [PluginInfo], format: PluginFormat, id: &str) -> Option<&'a PluginInfo> {
    match format {
        PluginFormat::Clap => {
            let suffix = format!("-{:x}", juce_string_hash(id));
            plugins
                .iter()
                .find(|p| p.format == format.name() && p.identifier.to_ascii_lowercase().ends_with(&suffix))
        }
    }
}

/// `juce::String::hashCode()`: 31 * hash + code point, over the UTF-32 code points, as u32.
fn juce_string_hash(text: &str) -> u32 {
    text.chars()
        .fold(0u32, |hash, c| hash.wrapping_mul(31).wrapping_add(c as u32))
}

// ---- ids ---------------------------------------------------------------------------------------

fn raw_track(id: TrackId) -> u64 {
    id.0.0
}
fn raw_clip(id: ClipId) -> u64 {
    id.0.0
}
fn raw_note(id: NoteId) -> u64 {
    id.0.0
}
fn raw_device(id: DeviceId) -> u64 {
    id.0.0
}
fn raw_tempo(id: TempoId) -> u64 {
    id.0.0
}

// ---- reading the model -------------------------------------------------------------------------

/// Tracks in display order, then any the order does not list (only a hand-built document has
/// those).
fn ordered_tracks(project: &Project) -> Vec<(TrackId, &TrackContent)> {
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
fn effective_tempo(project: &Project) -> BTreeMap<i64, (TempoId, u32)> {
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

fn content(project: &Project, track: TrackId) -> Option<&TrackContent> {
    project.track(track).map(|t| t.content.as_ref())
}

// ---- writing the engine ------------------------------------------------------------------------

struct Sync<'a> {
    engine: &'a Engine,
    blobs: BlobResolver<'a>,
    problems: Vec<Problem>,
    /// The engine's plugin list, read once per call when a plugin device is first met.
    plugins: Option<Vec<PluginInfo>>,
}

impl<'a> Sync<'a> {
    fn new(engine: &'a Engine, blobs: BlobResolver<'a>) -> Self {
        Self {
            engine,
            blobs,
            problems: Vec::new(),
            plugins: None,
        }
    }

    fn op(&mut self, before: &Project, op: &Op, after: &Project) -> Result<(), Error> {
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
                    // The engine has no call for a clip's name.
                    ClipProp::Name(_) => {}
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
                if content(after, *track).is_some_and(|c| c.device(*id).is_some()) {
                    self.problems.push(Problem::ParamNotMapped {
                        track: *track,
                        device: *id,
                        param: *param,
                    });
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
                match &device.kind {
                    DeviceKind::Plugin {
                        state: Some(blob), ..
                    } => {
                        if engine.has_device(raw_device(*id)) {
                            self.put_state(*track, *id, *blob);
                        }
                    }
                    DeviceKind::Plugin { state: None, .. } => {
                        // No saved state: the plugin as it starts. The engine cannot reset a
                        // plugin, so the device is made again in its place.
                        engine.device_remove(raw_device(*id))?;
                        let behind = self.device_predecessor(after, *track, *id);
                        self.put_device(*track, device, behind)?;
                    }
                    // The model ignores state for built-in devices.
                    DeviceKind::Builtin { .. } => {}
                }
            }
        }
        Ok(())
    }

    // ---- tempo

    fn put_tempo(&mut self, id: TempoId, tick: i64, milli_bpm: u32) -> Result<(), Error> {
        // The model clamps to its own range when it compiles the map; the engine's is narrower.
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
    fn put_track(&mut self, project: &Project, id: TrackId, after_track: u64) -> Result<(), Error> {
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

    fn put_clip(&mut self, track: TrackId, id: ClipId, clip: &mdaw_model::Clip) -> Result<(), Error> {
        let raw = raw_clip(id);
        if clip.length.0 < 1 {
            self.problems.push(Problem::ClipSkipped { track, clip: id });
            return self.engine.clip_remove(raw);
        }
        self.engine
            .clip_add(raw_track(track), raw, clip.start.0, clip.length.0, &clip.name)?;
        // A no-op when the clip was just added; sets the range when it was already there.
        self.engine.clip_set_range(raw, clip.start.0, clip.length.0)?;
        self.put_notes(track, id, clip.notes.iter().map(|(id, note)| (*id, note)))
    }

    fn put_notes<'n>(
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
    fn device_predecessor(&self, project: &Project, track: TrackId, device: DeviceId) -> u64 {
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
            .find(|&raw| self.engine.has_device(raw))
            .unwrap_or(0)
    }

    /// Adds a device with its bypass, parameters and state. Returns whether the engine has it.
    fn put_device(&mut self, track: TrackId, device: &Device, after_device: u64) -> Result<bool, Error> {
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
        for param in device.params.keys() {
            self.problems.push(Problem::ParamNotMapped {
                track,
                device: device.id,
                param: *param,
            });
        }
        if let DeviceKind::Plugin {
            state: Some(blob), ..
        } = &device.kind
        {
            self.put_state(track, device.id, *blob);
        }
        Ok(true)
    }

    /// A state the plugin refuses is a problem, not an error: the plugin is in the engine either
    /// way.
    fn put_state(&mut self, track: TrackId, device: DeviceId, blob: BlobHash) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use mdaw_model::{ActorId, IdGen, TempoPoint, Tick};

    #[test]
    fn juce_hash_matches_the_engines_identifier_for_the_test_synth() {
        // From the C bridge test: "CLAP-mdaw Test Synth-d11e4944-9de17b8".
        assert_eq!(
            format!("{:x}", juce_string_hash("org.mdaw.test-synth")),
            "9de17b8"
        );
        let list = vec![PluginInfo {
            format: "CLAP".into(),
            identifier: "CLAP-mdaw Test Synth-d11e4944-9de17b8".into(),
            name: "mdaw Test Synth".into(),
            vendor: String::new(),
            instrument: true,
        }];
        assert!(find_plugin(&list, PluginFormat::Clap, "org.mdaw.test-synth").is_some());
        assert!(find_plugin(&list, PluginFormat::Clap, "org.mdaw.other").is_none());
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
