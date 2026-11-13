//! What the session and the UI need from a sound engine, and nothing more.
//!
//! Two engines sit behind it during the move to Tracktion: [`crate::audio_old::OldAudio`], the
//! Rust engine (`mdaw-engine`, `mdaw-dsp`, `mdaw-plugin`), and with the `tracktion` feature
//! `crate::audio_tracktion::TracktionAudio`. Each plays the project on this machine only;
//! nothing here travels.

use std::any::Any;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mdaw_model::{BlobHash, DeviceId, PluginFormat, Project, Tick, TickRange, TrackId};

use crate::link::Waker;

/// A plugin's state saved from a running instance: its hash and its bytes.
pub type SavedState = (BlobHash, Arc<[u8]>);

/// How a track is heard on this client.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrackSound {
    /// No plugins: built-in devices only.
    Builtin,
    /// Its plugins run here.
    Live,
    /// A plugin of the track is not installed here (or plugins are off).
    NotInstalled,
    /// A plugin is installed but cannot run, as while its settings are not here.
    Silent,
    /// A plugin crashed or failed to load.
    Crashed,
    /// A plugin is starting, or the plugins are being looked for.
    Loading,
}

/// How one device is doing on this client.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DeviceStatus {
    /// A built-in device: always there.
    Builtin,
    Running,
    /// Being started, or waiting for the engine to take the latest project.
    Loading,
    /// Its plugin is not installed here, or plugins are off, or the format is not hosted.
    NotInstalled,
    /// Installed, but waiting for its settings.
    WaitingForState,
    /// It crashed, or failed to load: the text says what happened.
    Failed(String),
}

/// A plugin installed on this machine.
#[derive(Clone, PartialEq, Debug)]
pub struct PluginEntry {
    pub format: PluginFormat,
    /// The plugin's own id in its format: what the document stores.
    pub id: String,
    pub name: String,
    pub vendor: String,
    /// Makes sound from notes, as opposed to changing sound it is given.
    pub instrument: bool,
    /// Where it was found, for the user; may be empty.
    pub location: String,
}

/// A parameter of a running plugin, for the generic controls.
#[derive(Clone, PartialEq, Debug)]
pub struct ParamEntry {
    /// The plugin's own id for it (for CLAP the decimal id).
    pub id: String,
    pub name: String,
    /// In the plugin's own units, between `min` and `max`.
    pub value: f64,
    pub min: f64,
    pub max: f64,
    pub stepped: bool,
    /// The value as the plugin shows it, if it says; may be empty.
    pub text: String,
}

/// Plays live notes (computer keyboard, MIDI input, auditioning) from any thread, at once.
pub trait LiveSink: Send + Sync {
    /// Starts a note on `track`. `key` identifies it among the notes this client plays.
    fn note_on(&self, track: TrackId, key: u64, pitch: u8, vel: u8);
    /// Ends the note started under `key` with `pitch`.
    fn note_off(&self, track: TrackId, key: u64, pitch: u8);
}

/// The sound engine of this client.
pub trait Audio: Any {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// Who to wake when the engine has something for the main loop.
    fn set_waker(&mut self, _waker: Waker) {}

    // ---- the project

    /// The project as it should sound from now on: the session's view. Cheap when it did not
    /// change; the engine takes it over at most once per frame.
    fn set_project(&mut self, project: &Project);
    /// Housekeeping, once per UI frame, with the current view. Returns what the user should be
    /// told.
    fn frame(&mut self, project: &Project) -> Vec<String>;
    /// Chores between frames, as often as possible (plugins that live on this thread). Returns
    /// what the user should be told.
    fn pump(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// When the main loop has to run again although nothing happens on screen.
    fn next_frame_in(&self) -> Option<Duration> {
        None
    }

    // ---- local transport

    fn play(&mut self);
    fn stop(&mut self);
    fn seek(&mut self, tick: Tick);
    fn set_loop(&mut self, range: Option<TickRange>);
    /// What the transport was last told.
    fn is_playing(&self) -> bool;
    /// The playhead in ticks.
    fn position(&self) -> f64;
    /// The playhead in ticks, with when it is heard on `mdaw_engine::clock::now_micros`.
    fn position_heard(&self) -> (f64, i64);

    // ---- live notes and what is heard

    /// The live note player, for the keyboard and MIDI input.
    fn live(&self) -> Arc<dyn LiveSink>;
    /// The listener's own volume: not part of the project.
    fn set_output_gain(&mut self, gain: f32);
    fn output_gain(&self) -> f32;
    /// Output peak per channel since the last call.
    fn take_peak(&mut self) -> [f32; 2];
    /// The output, for the status line: device, rate, troubles.
    fn description(&self) -> String;

    // ---- plugins

    fn plugins_enabled(&self) -> bool;
    /// Whether the plugins on this machine are being looked for right now.
    fn scanning_plugins(&self) -> bool {
        false
    }
    fn plugin_catalog(&self) -> Vec<PluginEntry>;
    fn installed(&self, format: PluginFormat, id: &str) -> Option<PluginEntry>;
    fn device_status(&self, project: &Project, track: TrackId, device: DeviceId) -> DeviceStatus;
    fn track_sound(&self, project: &Project, track: TrackId) -> TrackSound;
    /// The first `limit` parameters of a running plugin, and how many it has.
    fn plugin_params(
        &mut self,
        track: TrackId,
        device: DeviceId,
        limit: usize,
    ) -> Option<(Vec<ParamEntry>, usize)>;
    /// Sets a parameter of a running plugin, as a user turning its knob (`value` in the units of
    /// [`ParamEntry::value`]). It reaches the document with the plugin's next saved state.
    fn set_plugin_param(&mut self, track: TrackId, device: DeviceId, param: &str, value: f64);
    fn has_editor(&self, track: TrackId, device: DeviceId) -> bool;
    fn editor_open(&self, track: TrackId, device: DeviceId) -> bool;
    /// Opens or closes the plugin's own window.
    fn toggle_editor(&mut self, track: TrackId, device: DeviceId, title: &str) -> Result<(), String>;
    /// Starts a crashed or failed plugin again, with the settings the document has.
    fn restart_plugin(&mut self, track: TrackId, device: DeviceId);

    // ---- plugin settings and the document

    /// A way to download blobs straight from the engine's own thread. An engine that takes it
    /// downloads what it needs itself, and [`Audio::wanted_blobs`] gives nothing.
    fn set_fetcher(&mut self, _fetcher: crate::link::Fetcher) {}
    /// State blobs the engine needs and that are not in `asked` yet.
    fn wanted_blobs(&mut self, asked: &HashSet<BlobHash>) -> Vec<BlobHash>;
    /// A blob asked for arrived, or (`None`) cannot be had.
    fn blob_arrived(&mut self, hash: BlobHash, bytes: Option<Arc<[u8]>>);
    /// The states of plugins whose settings changed here and differ from what `project` holds:
    /// they belong in the document. `now`: look at once, without waiting for the usual pace.
    fn changed_states(&mut self, project: &Project, now: bool) -> Vec<((TrackId, DeviceId), SavedState)>;

    /// Renders ticks `[from, to)` of the project to a wav file, without the device.
    fn render(&mut self, _path: &Path, _from: Tick, _to: Tick) -> Result<(), String> {
        Err("this engine does not render to files".into())
    }
}

/// How a track is heard, from the statuses of its devices.
pub fn track_sound_from(statuses: impl IntoIterator<Item = DeviceStatus>) -> TrackSound {
    let statuses: Vec<DeviceStatus> = statuses
        .into_iter()
        .filter(|s| *s != DeviceStatus::Builtin)
        .collect();
    if statuses.is_empty() {
        return TrackSound::Builtin;
    }
    if statuses.iter().any(|s| matches!(s, DeviceStatus::Failed(_))) {
        return TrackSound::Crashed;
    }
    if statuses.contains(&DeviceStatus::Loading) {
        return TrackSound::Loading;
    }
    if statuses.iter().all(|s| *s == DeviceStatus::Running) {
        return TrackSound::Live;
    }
    if statuses.contains(&DeviceStatus::NotInstalled) {
        return TrackSound::NotInstalled;
    }
    TrackSound::Silent
}
