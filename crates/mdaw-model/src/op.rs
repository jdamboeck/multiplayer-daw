use crate::doc::*;
use crate::ids::*;
use crate::time::{MAX_MILLI_BPM, MAX_TICK, MIN_MILLI_BPM, TempoPoint, Tick};
use serde::{Deserialize, Serialize};
use std::fmt;

/// One property-level edit addressed to stable IDs.
///
/// Ops never fail: an op whose target is missing, or an add whose ID already exists, does nothing
/// on every replica. The server checks ranges, limits and blobs before it sequences an op.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum Op {
    // Project.
    ProjectSetName {
        name: String,
    },
    MasterSetGain {
        gain: f32,
    },
    TempoPut {
        id: TempoId,
        point: TempoPoint,
    },
    TempoRemove {
        id: TempoId,
    },
    /// `after: None` puts the track first. `content` is empty for a new track and carries the old
    /// content when a removal is undone.
    TrackAdd {
        id: TrackId,
        shell: TrackShell,
        content: TrackContent,
        after: Option<TrackId>,
    },
    TrackMove {
        id: TrackId,
        after: Option<TrackId>,
    },
    TrackSetShell {
        id: TrackId,
        prop: ShellProp,
    },
    TrackRemove {
        id: TrackId,
    },

    // Track content.
    TrackSetMix {
        track: TrackId,
        prop: MixProp,
    },
    ClipAdd {
        track: TrackId,
        id: ClipId,
        clip: Clip,
    },
    ClipRemove {
        track: TrackId,
        id: ClipId,
    },
    ClipSet {
        track: TrackId,
        id: ClipId,
        prop: ClipProp,
    },
    NoteAdd {
        track: TrackId,
        clip: ClipId,
        id: NoteId,
        note: Note,
    },
    NoteRemove {
        track: TrackId,
        clip: ClipId,
        id: NoteId,
    },
    NoteSet {
        track: TrackId,
        clip: ClipId,
        id: NoteId,
        prop: NoteProp,
    },
    DeviceAdd {
        track: TrackId,
        device: Device,
        after: Option<DeviceId>,
    },
    DeviceRemove {
        track: TrackId,
        id: DeviceId,
    },
    DeviceMove {
        track: TrackId,
        id: DeviceId,
        after: Option<DeviceId>,
    },
    /// `value: None` returns the parameter to its default.
    DeviceSetParam {
        track: TrackId,
        id: DeviceId,
        param: u32,
        value: Option<f64>,
    },
    DeviceSetBypass {
        track: TrackId,
        id: DeviceId,
        bypass: bool,
    },
    /// Only meaningful for CLAP devices.
    DeviceSetState {
        track: TrackId,
        id: DeviceId,
        state: Option<BlobHash>,
    },
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum ShellProp {
    Name(String),
    Color(u32),
}

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub enum MixProp {
    Gain(f32),
    Pan(f32),
    Mute(bool),
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum ClipProp {
    Start(Tick),
    Length(Tick),
    Name(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum NoteProp {
    Start(Tick),
    Dur(Tick),
    Pitch(u8),
    Vel(u8),
}

/// Addresses one entity. Also used for presence ("X is editing this clip").
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum EntityRef {
    Project,
    Tempo(TempoId),
    Track(TrackId),
    Clip(TrackId, ClipId),
    Note(TrackId, ClipId, NoteId),
    Device(TrackId, DeviceId),
}

impl EntityRef {
    pub fn track(&self) -> Option<TrackId> {
        match *self {
            EntityRef::Track(t)
            | EntityRef::Clip(t, _)
            | EntityRef::Note(t, _, _)
            | EntityRef::Device(t, _) => Some(t),
            EntityRef::Project | EntityRef::Tempo(_) => None,
        }
    }

    /// True when `self` is `outer` or lives inside it, so removing `outer` removes `self`.
    pub fn is_within(&self, outer: &EntityRef) -> bool {
        if self == outer {
            return true;
        }
        match (*self, *outer) {
            (EntityRef::Clip(t, _), EntityRef::Track(o))
            | (EntityRef::Note(t, _, _), EntityRef::Track(o))
            | (EntityRef::Device(t, _), EntityRef::Track(o)) => t == o,
            (EntityRef::Note(t, c, _), EntityRef::Clip(ot, oc)) => t == ot && c == oc,
            _ => false,
        }
    }
}

/// Which property of an entity an op writes. Two ops conflict when entity and key are equal.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum PropKey {
    /// The entity is added or removed.
    Existence,
    /// Position among siblings.
    Order,
    Name,
    MasterGain,
    TempoPoint,
    Color,
    Gain,
    Pan,
    Mute,
    Start,
    Length,
    Dur,
    Pitch,
    Vel,
    Param(u32),
    Bypass,
    State,
}

impl Op {
    /// The entity this op writes to.
    pub fn entity(&self) -> EntityRef {
        match self {
            Op::ProjectSetName { .. } | Op::MasterSetGain { .. } => EntityRef::Project,
            Op::TempoPut { id, .. } | Op::TempoRemove { id } => EntityRef::Tempo(*id),
            Op::TrackAdd { id, .. }
            | Op::TrackMove { id, .. }
            | Op::TrackSetShell { id, .. }
            | Op::TrackRemove { id } => EntityRef::Track(*id),
            Op::TrackSetMix { track, .. } => EntityRef::Track(*track),
            Op::ClipAdd { track, id, .. } | Op::ClipRemove { track, id } | Op::ClipSet { track, id, .. } => {
                EntityRef::Clip(*track, *id)
            }
            Op::NoteAdd { track, clip, id, .. }
            | Op::NoteRemove { track, clip, id }
            | Op::NoteSet { track, clip, id, .. } => EntityRef::Note(*track, *clip, *id),
            Op::DeviceAdd { track, device, .. } => EntityRef::Device(*track, device.id),
            Op::DeviceRemove { track, id }
            | Op::DeviceMove { track, id, .. }
            | Op::DeviceSetParam { track, id, .. }
            | Op::DeviceSetBypass { track, id, .. }
            | Op::DeviceSetState { track, id, .. } => EntityRef::Device(*track, *id),
        }
    }

    /// The property this op writes.
    pub fn prop(&self) -> PropKey {
        match self {
            Op::ProjectSetName { .. } => PropKey::Name,
            Op::MasterSetGain { .. } => PropKey::MasterGain,
            Op::TempoPut { .. } => PropKey::TempoPoint,
            Op::TempoRemove { .. }
            | Op::TrackAdd { .. }
            | Op::TrackRemove { .. }
            | Op::ClipAdd { .. }
            | Op::ClipRemove { .. }
            | Op::NoteAdd { .. }
            | Op::NoteRemove { .. }
            | Op::DeviceAdd { .. }
            | Op::DeviceRemove { .. } => PropKey::Existence,
            Op::TrackMove { .. } | Op::DeviceMove { .. } => PropKey::Order,
            Op::TrackSetShell { prop, .. } => match prop {
                ShellProp::Name(_) => PropKey::Name,
                ShellProp::Color(_) => PropKey::Color,
            },
            Op::TrackSetMix { prop, .. } => match prop {
                MixProp::Gain(_) => PropKey::Gain,
                MixProp::Pan(_) => PropKey::Pan,
                MixProp::Mute(_) => PropKey::Mute,
            },
            Op::ClipSet { prop, .. } => match prop {
                ClipProp::Start(_) => PropKey::Start,
                ClipProp::Length(_) => PropKey::Length,
                ClipProp::Name(_) => PropKey::Name,
            },
            Op::NoteSet { prop, .. } => match prop {
                NoteProp::Start(_) => PropKey::Start,
                NoteProp::Dur(_) => PropKey::Dur,
                NoteProp::Pitch(_) => PropKey::Pitch,
                NoteProp::Vel(_) => PropKey::Vel,
            },
            Op::DeviceSetParam { param, .. } => PropKey::Param(*param),
            Op::DeviceSetBypass { .. } => PropKey::Bypass,
            Op::DeviceSetState { .. } => PropKey::State,
        }
    }

    /// True for ops that delete their entity and everything inside it.
    pub fn is_removal(&self) -> bool {
        matches!(
            self,
            Op::TempoRemove { .. }
                | Op::TrackRemove { .. }
                | Op::ClipRemove { .. }
                | Op::NoteRemove { .. }
                | Op::DeviceRemove { .. }
        )
    }

    /// Blobs the op refers to. The server requires them to be stored before it accepts the op.
    pub fn blobs(&self) -> Vec<BlobHash> {
        fn of_device(device: &Device) -> Option<BlobHash> {
            match &device.kind {
                DeviceKind::Plugin { state, .. } => *state,
                DeviceKind::Builtin { .. } => None,
            }
        }
        match self {
            Op::DeviceSetState { state, .. } => state.iter().copied().collect(),
            Op::DeviceAdd { device, .. } => of_device(device).into_iter().collect(),
            Op::TrackAdd { content, .. } => content.devices.iter().filter_map(of_device).collect(),
            _ => Vec::new(),
        }
    }

    /// Whether applying this op to `project` keeps it within the size limits. [`Op::validate`]
    /// checks what an op carries; this checks what it adds to what is there, so that many small
    /// ops cannot grow a clip or a track without end.
    pub fn fits(&self, project: &Project) -> Result<(), Invalid> {
        let content = |track: &TrackId| project.track(*track).map(|t| &t.content);
        let full = match self {
            Op::TrackAdd { id, .. } => !project.tracks.contains_key(id) && project.tracks.len() >= MAX_TRACKS,
            Op::TempoPut { id, .. } => {
                !project.tempo.contains_key(id) && project.tempo.len() >= MAX_TEMPO_POINTS
            }
            Op::ClipAdd { track, id, .. } => content(track)
                .is_some_and(|c| !c.clips.contains_key(id) && c.clips.len() >= MAX_CLIPS_PER_TRACK),
            Op::NoteAdd { track, clip, id, .. } => content(track)
                .and_then(|c| c.clips.get(clip))
                .is_some_and(|c| !c.notes.contains_key(id) && c.notes.len() >= MAX_NOTES_PER_CLIP),
            Op::DeviceAdd { track, .. } => {
                content(track).is_some_and(|c| c.devices.len() >= MAX_DEVICES_PER_TRACK)
            }
            Op::DeviceSetParam {
                track,
                id,
                param,
                value: Some(_),
            } => content(track)
                .and_then(|c| c.device(*id))
                .is_some_and(|d| !d.params.contains_key(param) && d.params.len() >= MAX_PARAMS_PER_DEVICE),
            _ => false,
        };
        if full {
            Err(Invalid("the limit of the project's size is reached"))
        } else {
            Ok(())
        }
    }

    /// Range checks the server runs before sequencing. `apply` itself accepts anything.
    pub fn validate(&self) -> Result<(), Invalid> {
        match self {
            Op::ProjectSetName { name } => check_name(name),
            Op::MasterSetGain { gain } => check_gain(*gain),
            Op::TempoPut { point, .. } => {
                check_tick(point.tick)?;
                if (MIN_MILLI_BPM..=MAX_MILLI_BPM).contains(&point.milli_bpm) {
                    Ok(())
                } else {
                    Err(Invalid("tempo out of range"))
                }
            }
            Op::TrackAdd { shell, content, .. } => {
                check_name(&shell.name)?;
                check_content(content)
            }
            Op::TrackSetShell {
                prop: ShellProp::Name(name),
                ..
            } => check_name(name),
            Op::TrackSetMix { prop, .. } => match *prop {
                MixProp::Gain(gain) => check_gain(gain),
                MixProp::Pan(pan) => check_pan(pan),
                MixProp::Mute(_) => Ok(()),
            },
            Op::ClipAdd { clip, .. } => check_clip(clip),
            Op::ClipSet { prop, .. } => match prop {
                ClipProp::Start(tick) => check_tick(*tick),
                ClipProp::Length(len) => check_length(*len),
                ClipProp::Name(name) => check_name(name),
            },
            Op::NoteAdd { note, .. } => check_note(note),
            Op::NoteSet { prop, .. } => match *prop {
                NoteProp::Start(tick) => check_tick(tick),
                NoteProp::Dur(dur) => check_length(dur),
                NoteProp::Pitch(pitch) => check_pitch(pitch),
                NoteProp::Vel(vel) => check_vel(vel),
            },
            Op::DeviceAdd { device, .. } => check_device(device),
            Op::DeviceSetParam { value: Some(v), .. } if !v.is_finite() => {
                Err(Invalid("parameter value is not finite"))
            }
            Op::TempoRemove { .. }
            | Op::TrackMove { .. }
            | Op::TrackSetShell { .. }
            | Op::TrackRemove { .. }
            | Op::ClipRemove { .. }
            | Op::NoteRemove { .. }
            | Op::DeviceRemove { .. }
            | Op::DeviceMove { .. }
            | Op::DeviceSetParam { .. }
            | Op::DeviceSetBypass { .. }
            | Op::DeviceSetState { .. } => Ok(()),
        }
    }
}

/// Why `Op::validate` refused an op.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Invalid(pub &'static str);

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Invalid {}

/// Longest name of a project, track or clip, in bytes of UTF-8.
pub const MAX_NAME_BYTES: usize = 128;

/// `name` followed by `suffix`, with `name` cut on a character boundary so that the whole fits
/// in [`MAX_NAME_BYTES`] and an op carrying it is valid. `suffix` is kept whole.
pub fn fit_name(name: &str, suffix: &str) -> String {
    let mut end = name.len().min(MAX_NAME_BYTES.saturating_sub(suffix.len()));
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &name[..end])
}
const MAX_PLUGIN_ID_BYTES: usize = 256;
const MAX_NOTES_PER_CLIP: usize = 50_000;
const MAX_CLIPS_PER_TRACK: usize = 10_000;
const MAX_TRACKS: usize = 1000;
const MAX_TEMPO_POINTS: usize = 10_000;
const MAX_DEVICES_PER_TRACK: usize = 32;
const MAX_PARAMS_PER_DEVICE: usize = 4096;
const MAX_GAIN: f32 = 4.0;

fn check_name(name: &str) -> Result<(), Invalid> {
    if name.len() <= MAX_NAME_BYTES {
        Ok(())
    } else {
        Err(Invalid("name too long"))
    }
}

fn check_gain(gain: f32) -> Result<(), Invalid> {
    if (0.0..=MAX_GAIN).contains(&gain) {
        Ok(())
    } else {
        Err(Invalid("gain out of range"))
    }
}

fn check_pan(pan: f32) -> Result<(), Invalid> {
    if (-1.0..=1.0).contains(&pan) {
        Ok(())
    } else {
        Err(Invalid("pan out of range"))
    }
}

fn check_tick(tick: Tick) -> Result<(), Invalid> {
    if (0..=MAX_TICK).contains(&tick.0) {
        Ok(())
    } else {
        Err(Invalid("position out of range"))
    }
}

fn check_length(len: Tick) -> Result<(), Invalid> {
    if (1..=MAX_TICK).contains(&len.0) {
        Ok(())
    } else {
        Err(Invalid("length out of range"))
    }
}

fn check_pitch(pitch: u8) -> Result<(), Invalid> {
    if pitch <= 127 {
        Ok(())
    } else {
        Err(Invalid("pitch out of range"))
    }
}

fn check_vel(vel: u8) -> Result<(), Invalid> {
    if (1..=127).contains(&vel) {
        Ok(())
    } else {
        Err(Invalid("velocity out of range"))
    }
}

fn check_note(note: &Note) -> Result<(), Invalid> {
    check_tick(note.start)?;
    check_length(note.dur)?;
    check_pitch(note.pitch)?;
    check_vel(note.vel)
}

fn check_clip(clip: &Clip) -> Result<(), Invalid> {
    check_tick(clip.start)?;
    check_length(clip.length)?;
    check_name(&clip.name)?;
    if clip.notes.len() > MAX_NOTES_PER_CLIP {
        return Err(Invalid("too many notes in one clip"));
    }
    clip.notes.values().try_for_each(check_note)
}

fn check_device(device: &Device) -> Result<(), Invalid> {
    if let DeviceKind::Plugin { id: plugin_id, .. } = &device.kind
        && (plugin_id.is_empty() || plugin_id.len() > MAX_PLUGIN_ID_BYTES)
    {
        return Err(Invalid("bad plugin id"));
    }
    if device.params.len() > MAX_PARAMS_PER_DEVICE {
        return Err(Invalid("too many parameters"));
    }
    if device.params.values().all(|v| v.is_finite()) {
        Ok(())
    } else {
        Err(Invalid("parameter value is not finite"))
    }
}

fn check_content(content: &TrackContent) -> Result<(), Invalid> {
    check_gain(content.gain)?;
    check_pan(content.pan)?;
    if content.clips.len() > MAX_CLIPS_PER_TRACK {
        return Err(Invalid("too many clips on one track"));
    }
    if content.devices.len() > MAX_DEVICES_PER_TRACK {
        return Err(Invalid("too many devices on one track"));
    }
    content.clips.values().try_for_each(|c| check_clip(c))?;
    content.devices.iter().try_for_each(check_device)
}
