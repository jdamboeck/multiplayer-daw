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

    /// Marks a track as checked out. Driven by `Checkout` events, not by ops.
    pub fn set_checkout(&mut self, track: TrackId, checkout: Option<Checkout>) -> bool {
        match self.tracks.get_mut(&track) {
            Some(t) if t.checkout != checkout => {
                Arc::make_mut(t).checkout = checkout;
                true
            }
            _ => false,
        }
    }

    /// Replaces a track's stem. Driven by `Commit` events.
    pub fn set_stem(&mut self, track: TrackId, stem: Option<Stem>) -> bool {
        match self.tracks.get_mut(&track) {
            Some(t) if t.stem != stem => {
                Arc::make_mut(t).stem = stem;
                true
            }
            _ => false,
        }
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Track {
    /// Project-level, always live.
    pub shell: TrackShell,
    /// Locked while the track is checked out.
    pub content: Arc<TrackContent>,
    pub checkout: Option<Checkout>,
    pub stem: Option<Stem>,
}

impl Track {
    pub fn new(shell: TrackShell, content: TrackContent) -> Self {
        Self {
            shell,
            content: Arc::new(content),
            checkout: None,
            stem: None,
        }
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct TrackShell {
    pub name: String,
    /// 0xRRGGBB.
    pub color: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Checkout {
    pub user: UserId,
    /// Server sequence number of the `Checkout` event.
    pub base_seq: u64,
}

/// Rendered audio of a committed track, for clients that cannot render it themselves.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct Stem {
    pub blob: BlobHash,
    /// `TempoMap::hash` at render time; a mismatch marks the stem stale.
    pub tempo_hash: u64,
    pub len_samples: u64,
    /// Multiply decoded samples by this to undo the peak scaling applied before encoding.
    pub gain: f32,
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

    pub fn has_clap_device(&self) -> bool {
        self.devices
            .iter()
            .any(|d| matches!(d.kind, DeviceKind::Clap { .. }))
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
    /// Local plugin: opaque state blob, heard by others through the track's stem.
    Clap {
        plugin_id: String,
        state: Option<BlobHash>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum BuiltinKind {
    Pocket,
}
