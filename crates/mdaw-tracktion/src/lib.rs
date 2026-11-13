//! Safe Rust API to the Tracktion Engine bridge (`mdaw-tracktion-sys`), and in [`apply`] the
//! mapping from the canonical project model to it.
//!
//! The engine holds a project *derived* from the application's canonical `mdaw_model::Project`
//! (docs/decisions/0001-canonical-project-state.md): [`apply::load_project`] builds it from
//! scratch, [`apply::apply_op`] carries one model op over incrementally, and
//! [`sync::sync_project`] brings it from one state of the project to another by their difference.
//!
//! The semantics of each call are those of `native/tracktion-bridge/include/mdaw_engine.h`.
//! Ids are the application's own 64-bit ids (0 means "none"); time is ticks at 960 per quarter.

pub mod apply;
pub mod sync;

use mdaw_tracktion_sys as sys;
use serde::Deserialize;
use std::ffi::{CStr, CString, c_char};
use std::fmt;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};

pub use apply::{BlobResolver, Problem, apply_op, find_plugin, load_project};
pub use sync::sync_project;

/// Ticks per quarter note, in the engine as in the model.
pub const TICKS_PER_QUARTER: i64 = sys::MDAW_TICKS_PER_QUARTER;

/// The ABI version of the linked native library.
pub fn abi_version() -> u32 {
    sys::mdaw_abi_version()
}

/// The plugin scanner (`mdaw-plugin-scan`) the build made, for [`Config::scanner_path`]: a path
/// fixed at compile time, inside the build folder (`target/native`). Right for tests and
/// development runs; a packaged application will ship the scanner next to its executable and
/// pass that path instead. `None` if the build gave none.
pub fn built_scanner_path() -> Option<&'static Path> {
    option_env!("MDAW_TRACKTION_BUILT_SCANNER").map(Path::new)
}

// ---- errors ------------------------------------------------------------------------------------

/// Mirrors `mdaw_status`, without `MDAW_OK`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Status {
    /// Null pointer, bad value; also a string with an interior NUL, caught on the Rust side.
    Argument,
    /// A plugin, a device or a file that had to exist does not.
    NotFound,
    /// The audio device could not be opened or changed.
    Device,
    /// The plugin could not be loaded.
    Plugin,
    /// A file could not be read or written.
    Io,
    /// Anything else, including a C++ exception caught at the boundary (and a second engine in
    /// one process). Also used for JSON the engine sent that did not parse.
    Internal,
    /// A status this crate does not know (a newer library).
    Unknown(i32),
}

impl Status {
    fn from_raw(raw: sys::mdaw_status) -> Self {
        match raw {
            sys::MDAW_ERR_ARGUMENT => Status::Argument,
            sys::MDAW_ERR_NOT_FOUND => Status::NotFound,
            sys::MDAW_ERR_DEVICE => Status::Device,
            sys::MDAW_ERR_PLUGIN => Status::Plugin,
            sys::MDAW_ERR_IO => Status::Io,
            sys::MDAW_ERR_INTERNAL => Status::Internal,
            other => Status::Unknown(other),
        }
    }
}

/// A failed engine call: its status and the engine's text for it (`mdaw_last_error`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Error {
    pub status: Status,
    pub message: String,
}

impl Error {
    fn new(status: Status, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// The calling thread's last error from the engine. Must be read on the thread that made
    /// the failed call, before it makes another one.
    fn last(status: Status) -> Self {
        let bytes = read_buffer(true, |buffer, capacity| {
            // SAFETY: `buffer` is valid for `capacity` bytes (see `read_buffer`).
            unsafe { sys::mdaw_last_error(buffer.cast::<c_char>(), capacity) }
        });
        Self::new(status, String::from_utf8_lossy(&bytes).into_owned())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "engine error ({:?}): {}", self.status, self.message)
    }
}

impl std::error::Error for Error {}

fn check(raw: sys::mdaw_status) -> Result<(), Error> {
    if raw == sys::MDAW_OK {
        Ok(())
    } else {
        Err(Error::last(Status::from_raw(raw)))
    }
}

fn c_string(text: &str) -> Result<CString, Error> {
    CString::new(text).map_err(|_| Error::new(Status::Argument, format!("interior NUL in {text:?}")))
}

fn c_path(path: &Path) -> Result<CString, Error> {
    let text = path
        .to_str()
        .ok_or_else(|| Error::new(Status::Argument, format!("path is not UTF-8: {}", path.display())))?;
    c_string(text)
}

fn parse_json<T: for<'de> Deserialize<'de>>(bytes: &[u8], what: &str) -> Result<T, Error> {
    serde_json::from_slice(bytes).map_err(|e| {
        Error::new(
            Status::Internal,
            format!("the engine's {what} did not parse as JSON: {e}"),
        )
    })
}

/// The buffer convention of the header: a call returns the full length it needs (for text,
/// excluding the NUL it writes after it); when that does not fit the capacity given, nothing
/// useful was written and the call is made again with a buffer that fits. Loops because the
/// value may grow between the two calls.
fn read_buffer(text: bool, mut call: impl FnMut(*mut u8, usize) -> usize) -> Vec<u8> {
    let terminator = usize::from(text);
    let mut buffer = vec![0u8; 1024];
    loop {
        let needed = call(buffer.as_mut_ptr(), buffer.len());
        if needed + terminator <= buffer.len() {
            buffer.truncate(needed);
            return buffer;
        }
        buffer.resize(needed + terminator, 0);
    }
}

// ---- values ------------------------------------------------------------------------------------

/// What [`Engine::has`] asks about (`mdaw_entity_kind`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EntityKind {
    Track,
    Clip,
    Device,
    Tempo,
}

impl EntityKind {
    fn raw(self) -> sys::mdaw_entity_kind {
        match self {
            EntityKind::Track => sys::MDAW_ENTITY_TRACK,
            EntityKind::Clip => sys::MDAW_ENTITY_CLIP,
            EntityKind::Device => sys::MDAW_ENTITY_DEVICE,
            EntityKind::Tempo => sys::MDAW_ENTITY_TEMPO,
        }
    }
}

/// How to create the engine.
#[derive(Clone, Debug)]
pub struct Config {
    /// Window titles, the engine's application name.
    pub app_name: String,
    /// Where the engine keeps its own settings; created if missing.
    pub settings_dir: PathBuf,
    /// Open the default audio device. If it cannot be opened the engine is still created; see
    /// [`EngineState::device_open`].
    pub open_audio_device: bool,
    /// "ALSA", "JACK", ... or `None` for the platform default.
    pub audio_backend: Option<String>,
    /// VST3, LV2 and CLAP as well as the built-in instruments.
    pub enable_plugins: bool,
    /// The plugin scanner executable (`mdaw-plugin-scan`, see [`built_scanner_path`]). With it,
    /// plugin files are examined in child processes, so a plugin that crashes or hangs while it
    /// is looked at cannot take the application down, and listing plugins or adding a device
    /// never loads a library just to identify it. `None`: examined in this process.
    pub scanner_path: Option<PathBuf>,
}

impl Config {
    /// No audio device, the default backend, plugins enabled.
    pub fn new(app_name: impl Into<String>, settings_dir: impl Into<PathBuf>) -> Self {
        Self {
            app_name: app_name.into(),
            settings_dir: settings_dir.into(),
            open_audio_device: false,
            audio_backend: None,
            enable_plugins: true,
            scanner_path: None,
        }
    }
}

/// One note for [`Engine::notes_put`]. `start_tick` is relative to the clip.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Note {
    pub id: u64,
    pub start_tick: i64,
    pub length_ticks: i64,
    pub pitch: u8,
    pub velocity: u8,
}

/// A plugin known to the engine (`mdaw_plugins_list`).
#[derive(Clone, PartialEq, Debug, Deserialize)]
pub struct PluginInfo {
    pub format: String,
    /// What [`Engine::device_add`] takes: the plugin's own id, the same on every machine. CLAP:
    /// the CLAP plugin id; LV2: the plugin URI; VST3: the class id as 32 hex digits.
    pub identifier: String,
    pub name: String,
    #[serde(default)]
    pub vendor: String,
    #[serde(default)]
    pub instrument: bool,
}

/// A plugin file that could not be examined (`mdaw_plugins_failed`): it crashed or hung the
/// scanner, or holds no plugin. It is not examined again until it changes.
#[derive(Clone, PartialEq, Eq, Debug, Deserialize)]
pub struct PluginFailure {
    pub format: String,
    /// The file (VST3 bundle, CLAP file) or, for LV2, the plugin URI.
    pub file: String,
    pub reason: String,
}

/// A device parameter (`mdaw_device_params`). Values are normalised to 0..1.
#[derive(Clone, PartialEq, Debug, Deserialize)]
pub struct ParamInfo {
    /// The plugin's own parameter id; for CLAP the decimal clap id.
    pub id: String,
    pub name: String,
    pub value: f64,
    /// `None` when the engine knows no default.
    #[serde(default)]
    pub default: Option<f64>,
    /// 0 for a continuous parameter.
    #[serde(default)]
    pub steps: i64,
    /// The plugin's own range where the format has one (CLAP): plain = min + value * (max - min).
    /// Otherwise 0 and 1.
    #[serde(default)]
    pub min: f64,
    #[serde(default = "one")]
    pub max: f64,
    /// The current value as the plugin shows it ("440 Hz").
    #[serde(default)]
    pub text: String,
}

fn one() -> f64 {
    1.0
}

impl ParamInfo {
    /// The normalised value of a plain value in the plugin's own units, clamped to the range.
    pub fn normalise(&self, plain: f64) -> f64 {
        let span = self.max - self.min;
        if span > 0.0 && plain.is_finite() {
            ((plain - self.min) / span).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

/// `mdaw_audio_devices`.
#[derive(Clone, PartialEq, Debug, Default, Deserialize)]
pub struct AudioDevices {
    #[serde(default)]
    pub backends: Vec<AudioBackend>,
    #[serde(default)]
    pub midi_inputs: Vec<String>,
    #[serde(default)]
    pub current: CurrentDevice,
}

#[derive(Clone, PartialEq, Debug, Default, Deserialize)]
pub struct AudioBackend {
    pub name: String,
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(default)]
    pub inputs: Vec<String>,
}

#[derive(Clone, PartialEq, Debug, Default, Deserialize)]
pub struct CurrentDevice {
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub sample_rate: f64,
    #[serde(default)]
    pub block_size: i32,
    #[serde(default)]
    pub open: bool,
}

/// `mdaw_engine_state`: cheap to read, meant for every UI frame.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct EngineState {
    /// What the application asked for; the engine keeps it so.
    pub playing: bool,
    pub position_tick: i64,
    pub position_seconds: f64,
    pub device_open: bool,
    pub sample_rate: f64,
    pub block_size: i32,
    /// Since the device was opened.
    pub xruns: i32,
    /// 0..1.
    pub cpu: f32,
    /// Master output since the last read, linear.
    pub peak_left: f32,
    pub peak_right: f32,
    /// How long ago `position_tick` and `position_seconds` were taken (about 30 times a second).
    pub position_age_seconds: f64,
    /// From the position being processed to its sound leaving the device (the device's output
    /// latency and one block); 0 without a device. `position_tick` is heard
    /// `output_latency_seconds - position_age_seconds` from now.
    pub output_latency_seconds: f64,
}

/// What the engine has to tell (`mdaw_engine_poll_event`). Changes caused by the application's
/// own calls are not reported.
#[derive(Clone, PartialEq, Debug)]
pub enum Event {
    /// Changed in the plugin's own window. `value` is normalised.
    ParamChanged {
        device: u64,
        param_id: String,
        value: f32,
    },
    /// The plugin's state changed otherwise (a preset loaded).
    StateChanged { device: u64 },
    /// The user closed the plugin's window.
    EditorClosed { device: u64 },
    /// The audio device or the device list changed.
    DeviceChanged,
    /// The audio callback has not returned for over a second.
    AudioStalled,
}

// ---- the engine ----------------------------------------------------------------------------------

/// The engine. One per process: a second [`Engine::new`] while one lives fails.
pub struct Engine {
    raw: NonNull<sys::mdaw_engine>,
    /// Calls made through this handle, for [`Engine::call_count`].
    calls: AtomicU64,
}

// SAFETY: the header allows every function to be called from any thread that is not a real-time
// audio thread; the engine carries each call out on its own message thread, so calls are
// serialised there, and it never calls back into Rust. The handle itself is never written from
// Rust after creation. `mdaw_last_error` is per calling thread and is read on the thread that
// made the failed call, right after it (see `check`). Destruction needs `Engine` by value
// (`Drop`), so no call can be running on another thread when it happens.
unsafe impl Send for Engine {}
// SAFETY: as above; every method takes `&self` and the C side serialises the calls.
unsafe impl Sync for Engine {}

impl Drop for Engine {
    fn drop(&mut self) {
        // SAFETY: the handle came from `mdaw_engine_create` and is destroyed exactly once.
        unsafe { sys::mdaw_engine_destroy(self.raw.as_ptr()) }
    }
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine").field("raw", &self.raw).finish()
    }
}

impl Engine {
    /// Creates the engine (and its message thread).
    pub fn new(config: Config) -> Result<Engine, Error> {
        let app_name = c_string(&config.app_name)?;
        // Made here as well as by the engine: this call copes with another process making a
        // parent folder at the same moment.
        std::fs::create_dir_all(&config.settings_dir).map_err(|e| Error {
            status: Status::Io,
            message: format!(
                "cannot create the settings folder {}: {e}",
                config.settings_dir.display()
            ),
        })?;
        let settings_dir = c_path(&config.settings_dir)?;
        let backend = config.audio_backend.as_deref().map(c_string).transpose()?;
        let scanner = config.scanner_path.as_deref().map(c_path).transpose()?;
        let raw_config = sys::mdaw_engine_config {
            abi_version: sys::MDAW_ABI_VERSION,
            app_name: app_name.as_ptr(),
            settings_dir: settings_dir.as_ptr(),
            open_audio_device: i32::from(config.open_audio_device),
            audio_backend: backend.as_ref().map_or(ptr::null(), |b| b.as_ptr()),
            enable_plugins: i32::from(config.enable_plugins),
            scanner_path: scanner.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
        };
        // SAFETY: the config and the strings it points to outlive the call.
        let raw = unsafe { sys::mdaw_engine_create(&raw_config) };
        // A failed create returns no status; the thread's last status has it.
        NonNull::new(raw)
            .map(|raw| Engine {
                raw,
                calls: AtomicU64::new(0),
            })
            .ok_or_else(|| {
                let status = match sys::mdaw_last_status() {
                    sys::MDAW_OK => Status::Internal,
                    raw => Status::from_raw(raw),
                };
                Error::last(status)
            })
    }

    fn ptr(&self) -> *mut sys::mdaw_engine {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.raw.as_ptr()
    }

    /// How many calls into the native engine this handle has made so far ([`Engine::state`]
    /// and [`Engine::poll_event`] included; a call whose result did not fit its buffer counts
    /// twice). For tests and diagnostics: it shows how much work an update took.
    pub fn call_count(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    fn text(&self, call: impl Fn(*mut sys::mdaw_engine, *mut c_char, usize) -> usize) -> Vec<u8> {
        read_buffer(true, |buffer, capacity| {
            call(self.ptr(), buffer.cast::<c_char>(), capacity)
        })
    }

    /// "Tracktion Engine v… / JUCE v…".
    pub fn versions(&self) -> String {
        let bytes = read_buffer(true, |buffer, capacity| {
            // SAFETY: `buffer` is valid for `capacity` bytes.
            unsafe { sys::mdaw_engine_versions(buffer.cast::<c_char>(), capacity) }
        });
        String::from_utf8_lossy(&bytes).into_owned()
    }

    // ---- the project

    /// Whether the engine has something of this kind under this id.
    pub fn has(&self, kind: EntityKind, id: u64) -> bool {
        // SAFETY: valid handle.
        unsafe { sys::mdaw_project_has(self.ptr(), kind.raw(), id) == 1 }
    }

    /// Removes everything: tracks, tempo points, mappings. Playback stops.
    pub fn clear(&self) -> Result<(), Error> {
        // SAFETY: valid handle.
        check(unsafe { sys::mdaw_project_clear(self.ptr()) })
    }

    pub fn tempo_put(&self, tempo_id: u64, tick: i64, milli_bpm: u32) -> Result<(), Error> {
        // SAFETY: valid handle.
        check(unsafe { sys::mdaw_tempo_put(self.ptr(), tempo_id, tick, milli_bpm) })
    }

    pub fn tempo_remove(&self, tempo_id: u64) -> Result<(), Error> {
        // SAFETY: valid handle.
        check(unsafe { sys::mdaw_tempo_remove(self.ptr(), tempo_id) })
    }

    /// Linear gain, 1 = unity.
    pub fn master_set_gain(&self, gain: f32) -> Result<(), Error> {
        // SAFETY: valid handle.
        check(unsafe { sys::mdaw_master_set_gain(self.ptr(), gain) })
    }

    /// `after_track` 0 puts the track first.
    pub fn track_add(&self, track: u64, after_track: u64, name: &str) -> Result<(), Error> {
        let name = c_string(name)?;
        // SAFETY: valid handle; the string outlives the call.
        check(unsafe { sys::mdaw_track_add(self.ptr(), track, after_track, name.as_ptr()) })
    }

    pub fn track_remove(&self, track: u64) -> Result<(), Error> {
        // SAFETY: valid handle.
        check(unsafe { sys::mdaw_track_remove(self.ptr(), track) })
    }

    pub fn track_move(&self, track: u64, after_track: u64) -> Result<(), Error> {
        // SAFETY: valid handle.
        check(unsafe { sys::mdaw_track_move(self.ptr(), track, after_track) })
    }

    pub fn track_set_name(&self, track: u64, name: &str) -> Result<(), Error> {
        let name = c_string(name)?;
        // SAFETY: valid handle; the string outlives the call.
        check(unsafe { sys::mdaw_track_set_name(self.ptr(), track, name.as_ptr()) })
    }

    /// Linear gain, pan -1..1.
    pub fn track_set_mix(&self, track: u64, gain: f32, pan: f32, mute: bool) -> Result<(), Error> {
        // SAFETY: valid handle.
        check(unsafe { sys::mdaw_track_set_mix(self.ptr(), track, gain, pan, i32::from(mute)) })
    }

    /// Plays a note on the track's devices at once, as a key on a keyboard does (velocity 0
    /// ends it). Does not wait for the engine's thread; nothing is heard without an open device
    /// or for a track the engine does not have, and that is not an error.
    pub fn track_live_note(&self, track: u64, pitch: u8, velocity: u8) -> Result<(), Error> {
        // SAFETY: valid handle.
        check(unsafe { sys::mdaw_track_live_note(self.ptr(), track, pitch, velocity) })
    }

    pub fn clip_add(
        &self,
        track: u64,
        clip: u64,
        start_tick: i64,
        length_ticks: i64,
        name: &str,
    ) -> Result<(), Error> {
        let name = c_string(name)?;
        // SAFETY: valid handle; the string outlives the call.
        check(unsafe { sys::mdaw_clip_add(self.ptr(), track, clip, start_tick, length_ticks, name.as_ptr()) })
    }

    pub fn clip_remove(&self, clip: u64) -> Result<(), Error> {
        // SAFETY: valid handle.
        check(unsafe { sys::mdaw_clip_remove(self.ptr(), clip) })
    }

    pub fn clip_set_range(&self, clip: u64, start_tick: i64, length_ticks: i64) -> Result<(), Error> {
        // SAFETY: valid handle.
        check(unsafe { sys::mdaw_clip_set_range(self.ptr(), clip, start_tick, length_ticks) })
    }

    pub fn clip_set_name(&self, clip: u64, name: &str) -> Result<(), Error> {
        let name = c_string(name)?;
        // SAFETY: valid handle; the string outlives the call.
        check(unsafe { sys::mdaw_clip_set_name(self.ptr(), clip, name.as_ptr()) })
    }

    /// Adds the notes that are new and updates those whose id the clip already has.
    pub fn notes_put(&self, clip: u64, notes: &[Note]) -> Result<(), Error> {
        let raw: Vec<sys::mdaw_note> = notes
            .iter()
            .map(|n| sys::mdaw_note {
                id: n.id,
                start_tick: n.start_tick,
                length_ticks: n.length_ticks,
                pitch: n.pitch,
                velocity: n.velocity,
            })
            .collect();
        // SAFETY: valid handle; `raw` holds `raw.len()` notes (a dangling but aligned pointer
        // for none, which the C side does not read).
        check(unsafe { sys::mdaw_notes_put(self.ptr(), clip, raw.as_ptr(), raw.len()) })
    }

    pub fn notes_remove(&self, clip: u64, note_ids: &[u64]) -> Result<(), Error> {
        // SAFETY: valid handle; the slice holds `len` ids.
        check(unsafe { sys::mdaw_notes_remove(self.ptr(), clip, note_ids.as_ptr(), note_ids.len()) })
    }

    /// `format` "builtin" with `identifier` "4osc" is the engine's own synth. Otherwise "VST3",
    /// "LV2" or "CLAP" and a [`PluginInfo::identifier`]. Not installed: [`Status::NotFound`];
    /// failed to load: [`Status::Plugin`]; in both cases no device is created.
    pub fn device_add(
        &self,
        track: u64,
        device: u64,
        after_device: u64,
        format: &str,
        identifier: &str,
    ) -> Result<(), Error> {
        let format = c_string(format)?;
        let identifier = c_string(identifier)?;
        // SAFETY: valid handle; the strings outlive the call.
        check(unsafe {
            sys::mdaw_device_add(
                self.ptr(),
                track,
                device,
                after_device,
                format.as_ptr(),
                identifier.as_ptr(),
            )
        })
    }

    pub fn device_remove(&self, device: u64) -> Result<(), Error> {
        // SAFETY: valid handle.
        check(unsafe { sys::mdaw_device_remove(self.ptr(), device) })
    }

    pub fn device_move(&self, device: u64, after_device: u64) -> Result<(), Error> {
        // SAFETY: valid handle.
        check(unsafe { sys::mdaw_device_move(self.ptr(), device, after_device) })
    }

    pub fn device_set_bypass(&self, device: u64, bypass: bool) -> Result<(), Error> {
        // SAFETY: valid handle.
        check(unsafe { sys::mdaw_device_set_bypass(self.ptr(), device, i32::from(bypass)) })
    }

    /// `value` normalised 0..1. Reaches the plugin with the next block it processes.
    pub fn device_set_param(&self, device: u64, param_id: &str, value: f32) -> Result<(), Error> {
        let param_id = c_string(param_id)?;
        // SAFETY: valid handle; the string outlives the call.
        check(unsafe { sys::mdaw_device_set_param(self.ptr(), device, param_id.as_ptr(), value) })
    }

    pub fn device_get_param(&self, device: u64, param_id: &str) -> Result<f32, Error> {
        let param_id = c_string(param_id)?;
        let mut value = 0.0f32;
        // SAFETY: valid handle; the string and `value` outlive the call.
        check(unsafe { sys::mdaw_device_get_param(self.ptr(), device, param_id.as_ptr(), &mut value) })?;
        Ok(value)
    }

    /// The device's parameters; empty for a device that does not exist.
    pub fn device_params(&self, device: u64) -> Result<Vec<ParamInfo>, Error> {
        let json = self.device_params_json(device);
        parse_json(&json, "parameter list")
    }

    fn device_params_json(&self, device: u64) -> Vec<u8> {
        // SAFETY: valid handle; `buffer` is valid for `capacity` bytes.
        self.text(|e, buffer, capacity| unsafe { sys::mdaw_device_params(e, device, buffer, capacity) })
    }

    /// How the plugin shows this normalised value of the parameter.
    pub fn device_param_text(&self, device: u64, param_id: &str, value: f32) -> Result<String, Error> {
        if !(0.0..=1.0).contains(&value) {
            return Err(Error::new(Status::Argument, "parameter value must be 0..1"));
        }
        let param = c_string(param_id)?;
        let bytes = self.text(|e, buffer, capacity| {
            // SAFETY: valid handle; the string outlives the call; `buffer` is valid for
            // `capacity` bytes.
            unsafe { sys::mdaw_device_param_text(e, device, param.as_ptr(), value, buffer, capacity) }
        });
        if bytes.is_empty() {
            // Empty text, or a failure: a length gives no status, so ask in a way that does.
            self.device_get_param(device, param_id)?;
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The plugin's opaque state, flushed from the plugin first. Empty if the device does not
    /// exist or has no state.
    pub fn device_get_state(&self, device: u64) -> Vec<u8> {
        read_buffer(false, |buffer, capacity| {
            // SAFETY: valid handle; `buffer` is valid for `capacity` bytes.
            unsafe { sys::mdaw_device_get_state(self.ptr(), device, buffer, capacity) }
        })
    }

    pub fn device_set_state(&self, device: u64, state: &[u8]) -> Result<(), Error> {
        // SAFETY: valid handle; the slice holds `len` bytes.
        check(unsafe { sys::mdaw_device_set_state(self.ptr(), device, state.as_ptr(), state.len()) })
    }

    /// Back to the state the plugin has when freshly loaded; the device keeps its id and place.
    pub fn device_reset_state(&self, device: u64) -> Result<(), Error> {
        // SAFETY: valid handle.
        check(unsafe { sys::mdaw_device_reset_state(self.ptr(), device) })
    }

    /// Shows or hides the plugin's own window.
    pub fn device_show_editor(&self, device: u64, show: bool) -> Result<(), Error> {
        // SAFETY: valid handle.
        check(unsafe { sys::mdaw_device_show_editor(self.ptr(), device, i32::from(show)) })
    }

    // ---- plugins on this machine

    /// Scans the usual folders for one format ("VST3", "LV2", "CLAP"). Blocks until done. With
    /// a scanner ([`Config::scanner_path`]) each file is examined in a child process with a
    /// time limit (30 s; `MDAW_SCAN_TIMEOUT_MS` overrides it).
    pub fn plugins_scan(&self, format: &str) -> Result<(), Error> {
        let format = c_string(format)?;
        // SAFETY: valid handle; the string outlives the call.
        check(unsafe { sys::mdaw_plugins_scan(self.ptr(), format.as_ptr()) })
    }

    /// The plugins the engine knows from scans (this run's or earlier ones') and can load now.
    pub fn plugins(&self) -> Result<Vec<PluginInfo>, Error> {
        // SAFETY: valid handle; `buffer` is valid for `capacity` bytes.
        let json = self.text(|e, buffer, capacity| unsafe { sys::mdaw_plugins_list(e, buffer, capacity) });
        parse_json(&json, "plugin list")
    }

    /// The plugin files that could not be examined, with the reason, as remembered from scans
    /// (this run's or earlier ones').
    pub fn plugins_failed(&self) -> Result<Vec<PluginFailure>, Error> {
        // SAFETY: valid handle; `buffer` is valid for `capacity` bytes.
        let json = self.text(|e, buffer, capacity| unsafe { sys::mdaw_plugins_failed(e, buffer, capacity) });
        parse_json(&json, "list of failed plugin files")
    }

    // ---- local playback

    pub fn transport_play(&self) -> Result<(), Error> {
        // SAFETY: valid handle.
        check(unsafe { sys::mdaw_transport_play(self.ptr()) })
    }

    pub fn transport_stop(&self) -> Result<(), Error> {
        // SAFETY: valid handle.
        check(unsafe { sys::mdaw_transport_stop(self.ptr()) })
    }

    pub fn transport_seek(&self, tick: i64) -> Result<(), Error> {
        // SAFETY: valid handle.
        check(unsafe { sys::mdaw_transport_seek(self.ptr(), tick) })
    }

    /// `end_tick <= start_tick` switches looping off.
    pub fn transport_set_loop(&self, start_tick: i64, end_tick: i64) -> Result<(), Error> {
        // SAFETY: valid handle.
        check(unsafe { sys::mdaw_transport_set_loop(self.ptr(), start_tick, end_tick) })
    }

    /// Cheap: reads atomics, does not wait for the engine's thread.
    pub fn state(&self) -> EngineState {
        let mut raw = sys::mdaw_engine_state::default();
        // SAFETY: valid handle; `raw` is a valid place to write.
        unsafe { sys::mdaw_engine_state_read(self.ptr(), &mut raw) };
        EngineState {
            playing: raw.playing != 0,
            position_tick: raw.position_tick,
            position_seconds: raw.position_seconds,
            device_open: raw.device_open != 0,
            sample_rate: raw.sample_rate,
            block_size: raw.block_size,
            xruns: raw.xruns,
            cpu: raw.cpu,
            peak_left: raw.peak_left,
            peak_right: raw.peak_right,
            position_age_seconds: raw.position_age_seconds,
            output_latency_seconds: raw.output_latency_seconds,
        }
    }

    pub fn audio_devices(&self) -> Result<AudioDevices, Error> {
        // SAFETY: valid handle; `buffer` is valid for `capacity` bytes.
        let json = self.text(|e, buffer, capacity| unsafe { sys::mdaw_audio_devices(e, buffer, capacity) });
        parse_json(&json, "audio device list")
    }

    /// `None` leaves a setting as it is.
    pub fn audio_device_set(
        &self,
        backend: Option<&str>,
        output_device: Option<&str>,
        sample_rate: Option<f64>,
        block_size: Option<i32>,
    ) -> Result<(), Error> {
        let backend = backend.map(c_string).transpose()?;
        let output = output_device.map(c_string).transpose()?;
        // SAFETY: valid handle; the strings outlive the call.
        check(unsafe {
            sys::mdaw_audio_device_set(
                self.ptr(),
                backend.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
                output.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
                sample_rate.unwrap_or(0.0),
                block_size.unwrap_or(0),
            )
        })
    }

    /// Renders ticks `[start_tick, end_tick)` to a 24-bit stereo wav file, faster than real time.
    pub fn render(
        &self,
        wav_path: &Path,
        start_tick: i64,
        end_tick: i64,
        sample_rate: f64,
    ) -> Result<(), Error> {
        let path = c_path(wav_path)?;
        // SAFETY: valid handle; the string outlives the call.
        check(unsafe { sys::mdaw_render(self.ptr(), path.as_ptr(), start_tick, end_tick, sample_rate) })
    }

    // ---- events

    /// The next event, if there is one. Call until `None`, every frame.
    pub fn poll_event(&self) -> Option<Event> {
        let mut raw = sys::mdaw_event::default();
        // SAFETY: valid handle; `raw` is a valid place to write.
        if unsafe { sys::mdaw_engine_poll_event(self.ptr(), &mut raw) } == 0 {
            return None;
        }
        Some(match raw.kind {
            sys::MDAW_EVENT_PARAM_CHANGED => Event::ParamChanged {
                device: raw.device,
                param_id: param_id_text(&raw.param_id),
                value: raw.value,
            },
            sys::MDAW_EVENT_STATE_CHANGED => Event::StateChanged { device: raw.device },
            sys::MDAW_EVENT_EDITOR_CLOSED => Event::EditorClosed { device: raw.device },
            sys::MDAW_EVENT_AUDIO_STALLED => Event::AudioStalled,
            // DEVICE_CHANGED, and anything a newer library adds: the safest reading is "look
            // at the devices again".
            _ => Event::DeviceChanged,
        })
    }
}

fn param_id_text(raw: &[c_char; 64]) -> String {
    let bytes: Vec<u8> = raw.iter().map(|&c| c as u8).collect();
    match CStr::from_bytes_until_nul(&bytes) {
        Ok(text) => text.to_string_lossy().into_owned(),
        Err(_) => String::from_utf8_lossy(&bytes).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_buffer_grows_once_for_text() {
        let value = vec![b'x'; 3000];
        let mut calls = 0;
        let out = read_buffer(true, |buffer, capacity| {
            calls += 1;
            if value.len() < capacity {
                // SAFETY: the buffer has room for the text and its NUL.
                unsafe {
                    ptr::copy_nonoverlapping(value.as_ptr(), buffer, value.len());
                    *buffer.add(value.len()) = 0;
                }
            }
            value.len()
        });
        assert_eq!(out, value);
        assert_eq!(calls, 2);
    }

    #[test]
    fn read_buffer_exact_fit_for_bytes() {
        let value = vec![7u8; 1024];
        let out = read_buffer(false, |buffer, capacity| {
            if value.len() <= capacity {
                // SAFETY: the buffer has room for the bytes.
                unsafe { ptr::copy_nonoverlapping(value.as_ptr(), buffer, value.len()) };
            }
            value.len()
        });
        assert_eq!(out, value);
        assert!(read_buffer(false, |_, _| 0).is_empty());
    }

    #[test]
    fn interior_nul_is_an_argument_error() {
        assert_eq!(c_string("a\0b").unwrap_err().status, Status::Argument);
    }

    #[test]
    fn param_id_without_nul() {
        let raw = [b'7' as c_char; 64];
        assert_eq!(param_id_text(&raw), "7".repeat(64));
    }

    #[test]
    fn json_values_parse() {
        let params: Vec<ParamInfo> = parse_json(
            br#"[{"id": "1", "name": "Balance", "value": 0.5, "default": 0.5, "steps": 0, "min": -1.0, "max": 1.0, "text": "+0.00"},
                 {"id": "x", "name": "N", "value": 1, "default": null, "steps": 3}]"#,
            "test",
        )
        .unwrap();
        assert_eq!(params[1].default, None);
        assert_eq!(params[1].steps, 3);
        assert_eq!((params[1].min, params[1].max), (0.0, 1.0));
        assert_eq!(params[0].text, "+0.00");
        assert_eq!(params[0].normalise(-1.0), 0.0);
        assert_eq!(params[0].normalise(0.5), 0.75);
        assert_eq!(params[0].normalise(7.0), 1.0);
        let plugins: Vec<PluginInfo> = parse_json(
            br#"[{"format": "CLAP", "identifier": "org.example.a", "name": "a", "vendor": "v", "instrument": true}]"#,
            "test",
        )
        .unwrap();
        assert!(plugins[0].instrument);
        let devices: AudioDevices = parse_json(
            br#"{"backends": [{"name": "ALSA", "outputs": ["o"], "inputs": []}], "midi_inputs": [],
                 "current": {"backend": "ALSA", "output": "", "sample_rate": 0.0, "block_size": 0, "open": false}}"#,
            "test",
        )
        .unwrap();
        assert_eq!(devices.backends[0].outputs, vec!["o".to_string()]);
    }
}
