use crate::ids::*;
use crate::time::{TempoMap, TempoPoint, Tick, TickRange};
use imbl::{OrdMap, Vector};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The whole shared document. Cloning is O(1); the UI and engine work on snapshots.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub tempo: OrdMap<TempoId, TempoPoint>,
    pub master_gain: f32,
    pub track_order: Vector<TrackId>,
    pub tracks: OrdMap<TrackId, Arc<Track>>,
}

impl Default for Project {
    fn default() -> Self {
        Self {
            name: "Untitled".into(),
            tempo: OrdMap::new(),
            master_gain: 1.0,
            track_order: Vector::new(),
            tracks: OrdMap::new(),
        }
    }
}

impl Project {
    pub fn track(&self, id: TrackId) -> Option<&Arc<Track>> {
        self.tracks.get(&id)
    }

    /// Tracks in display order.
    pub fn ordered_tracks(&self) -> impl Iterator<Item = (TrackId, &Arc<Track>)> {
        self.track_order
            .iter()
            .filter_map(|id| self.tracks.get(id).map(|t| (*id, t)))
    }

    pub fn tempo_map(&self) -> TempoMap {
        TempoMap::compile(&self.tempo)
    }

    /// End of the last clip on any track.
    pub fn end(&self) -> Tick {
        self.tracks
            .values()
            .filter_map(|t| t.content.extent())
            .map(|r| r.end)
            .max()
            .unwrap_or_default()
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Track {
    /// Name and colour.
    pub shell: TrackShell,
    /// Mix, clips and devices.
    pub content: Arc<TrackContent>,
}

impl Track {
    pub fn new(shell: TrackShell, content: TrackContent) -> Self {
        Self {
            shell,
            content: Arc::new(content),
        }
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct TrackShell {
    pub name: String,
    /// 0xRRGGBB.
    pub color: u32,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct TrackContent {
    pub gain: f32,
    pub pan: f32,
    pub mute: bool,
    pub clips: OrdMap<ClipId, Arc<Clip>>,
    pub devices: Vector<Device>,
}

impl Default for TrackContent {
    fn default() -> Self {
        Self {
            gain: 1.0,
            pan: 0.0,
            mute: false,
            clips: OrdMap::new(),
            devices: Vector::new(),
        }
    }
}

impl TrackContent {
    pub fn device(&self, id: DeviceId) -> Option<&Device> {
        self.devices.iter().find(|d| d.id == id)
    }

    /// From the start of the first clip to the end of the last.
    pub fn extent(&self) -> Option<TickRange> {
        self.clips.values().map(|c| c.range()).reduce(TickRange::union)
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Clip {
    pub start: Tick,
    pub length: Tick,
    pub name: String,
    pub notes: OrdMap<NoteId, Note>,
}

impl Clip {
    pub fn range(&self) -> TickRange {
        TickRange::new(self.start, self.start + self.length)
    }
}

/// `start` is relative to the clip. Notes starting at or after the clip's length do not sound;
/// notes running past it are cut at the clip's end.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Note {
    pub start: Tick,
    pub dur: Tick,
    pub pitch: u8,
    pub vel: u8,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Device {
    pub id: DeviceId,
    pub kind: DeviceKind,
    /// Parameter values by parameter ID. A missing entry means the parameter's default.
    pub params: OrdMap<u32, f64>,
    pub bypass: bool,
}

impl Device {
    pub fn new(id: DeviceId, kind: DeviceKind) -> Self {
        Self {
            id,
            kind,
            params: OrdMap::new(),
            bypass: false,
        }
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum DeviceKind {
    /// Deterministic built-in: only params are synced and every client renders it.
    Builtin { kind: BuiltinKind, dsp_version: u16 },
    /// A plugin installed on the client's machine: its state is an opaque blob. A client that
    /// lacks the plugin gets no sound from the device.
    Plugin {
        format: PluginFormat,
        /// The plugin's own identifier within its format, compared as an opaque string.
        id: String,
        state: Option<BlobHash>,
    },
}

/// The plugin standards a device can come from. The device's `id` is the plugin's own id in its
/// format, the same on every machine: for CLAP the plugin id, for VST3 the class id as 32 hex
/// digits, for LV2 the plugin URI. A client that cannot host a format treats its devices as not
/// installed. New variants go at the end: the wire and store formats number them.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub enum PluginFormat {
    Clap,
    Vst3,
    Lv2,
}

impl PluginFormat {
    pub const ALL: [PluginFormat; 3] = [PluginFormat::Clap, PluginFormat::Vst3, PluginFormat::Lv2];

    /// The format's usual name, which is also how the audio engine names it.
    pub fn name(self) -> &'static str {
        match self {
            PluginFormat::Clap => "CLAP",
            PluginFormat::Vst3 => "VST3",
            PluginFormat::Lv2 => "LV2",
        }
    }

    /// The format with this [`name`](Self::name).
    pub fn from_name(name: &str) -> Option<PluginFormat> {
        Self::ALL.into_iter().find(|f| f.name() == name)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum BuiltinKind {
    Pocket,
}
