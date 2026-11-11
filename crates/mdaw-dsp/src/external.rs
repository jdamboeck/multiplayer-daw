//! Devices the graph runs without knowing what they are: plugins, hosted elsewhere. The graph
//! sees only [`ExternalDevice`]; where one comes from is the business of a [`PluginSource`].

use mdaw_model::{BlobHash, DeviceId, PluginFormat, Stem, TrackId};
use std::sync::Arc;

/// Where a block of audio sits on the timeline, for devices that follow the transport.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct BlockTime {
    /// The transport runs.
    pub playing: bool,
    /// Timeline position of the block's first frame, in ticks.
    pub tick: f64,
    /// Seconds from the start of the timeline to the block's first frame.
    pub seconds: f64,
    /// Tempo at the block's start.
    pub bpm: f64,
}

/// A device processed in the audio thread through this interface alone. Every method may run in
/// the audio callback: none may allocate, lock or block.
pub trait ExternalDevice: Send {
    /// Starts a note. `key` identifies it for [`ExternalDevice::note_off`].
    fn note_on(&mut self, key: u64, pitch: u8, vel: u8);
    fn note_off(&mut self, key: u64);
    /// Ends the sounding notes whose key matches.
    fn release_where(&mut self, filter: &dyn Fn(u64) -> bool);
    /// Processes one block of stereo in place: a device with audio input takes `left` and
    /// `right` as its input and replaces them with its output; one without adds its output to
    /// them.
    fn process(&mut self, left: &mut [f32], right: &mut [f32], time: &BlockTime);
    /// The device leaves the graph: called on the audio thread, after its last `process` and
    /// before it is handed back for disposal.
    fn retire(&mut self) {}
}

/// A plugin device as the document describes it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PluginRef<'a> {
    pub format: PluginFormat,
    pub id: &'a str,
    pub state: Option<BlobHash>,
}

/// Provides what the graph cannot make itself. Asked while a graph is built, off the audio
/// thread.
pub trait PluginSource {
    /// A running processor for a plugin device, or `None` if it cannot be heard here: the plugin
    /// is missing, or its state has not arrived yet.
    fn device(
        &mut self,
        track: TrackId,
        device: DeviceId,
        plugin: PluginRef<'_>,
    ) -> Option<Box<dyn ExternalDevice>>;
    /// A stem as interleaved stereo at the graph's rate, its gain applied, or `None` if it is not
    /// at hand yet.
    fn stem(&mut self, stem: &Stem) -> Option<Arc<[f32]>>;
}

/// No plugins and no stems: tracks with plugins are silent.
pub struct NoPlugins;

impl PluginSource for NoPlugins {
    fn device(&mut self, _: TrackId, _: DeviceId, _: PluginRef<'_>) -> Option<Box<dyn ExternalDevice>> {
        None
    }

    fn stem(&mut self, _: &Stem) -> Option<Arc<[f32]>> {
        None
    }
}
