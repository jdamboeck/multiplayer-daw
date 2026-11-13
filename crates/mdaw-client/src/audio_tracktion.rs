//! Tracktion Engine behind [`Audio`], through the bridge (`mdaw-tracktion`).
//!
//! There is one engine per process (JUCE), so one `TracktionAudio`. Every call into the engine
//! that changes something, or may take long (loading a plugin, scanning, rendering), runs on a
//! worker thread owned by this value, in the order it was asked for; the UI thread only reads
//! the engine's state and events, which never wait, and the results the worker leaves in
//! [`Shared`]. So a plugin that takes seconds to load never freezes the window.
//!
//! The project reaches the engine by difference: the worker keeps the project it last brought
//! the engine to and hands both states to [`mdaw_tracktion::sync_project`]. Requests to follow
//! a new view are coalesced: however often the view changes, the worker syncs once to the latest.
//!
//! Plugin settings travel the other way as state blobs. A parameter changed in a plugin's own
//! window, a preset loaded there, or a generic slider moved here marks the device; 500 ms after
//! the last such change (at most 2 s after the first) the worker reads the plugin's state, and
//! the session puts it into the document as an ordinary `DeviceSetState` edit. The worker notes
//! that the plugin holds that state already, so the edit coming back through the view changes
//! nothing in the engine; a state that arrives from elsewhere is applied with
//! `device_set_state`, which the bridge does not report back as a change, and a captured state
//! equal to what the document holds is never submitted.

use std::any::Any;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use mdaw_engine::clock;
use mdaw_model::{
    BlobHash, DeviceId, DeviceKind, Op, PluginFormat, Project, Tick, TickRange, TrackContent, TrackId,
};
use mdaw_tracktion::{Config, Engine, EngineState, EntityKind, Event, Problem};

use crate::audio::{
    Audio, DeviceStatus, LiveSink, ParamEntry, PluginEntry, SavedState, TrackSound, track_sound_from,
};
use crate::link::Waker;

/// A plugin's settings are read this long after its last change...
const CAPTURE_QUIET: Duration = Duration::from_millis(500);
/// ...but no later than this after the first one.
const CAPTURE_LATEST: Duration = Duration::from_secs(2);
/// The sample rate of renders (one rate for all, so plugins are not prepared anew each time).
const RENDER_RATE: f64 = 48_000.0;
/// Parameter values shown in the device tab are read again this often.
const PARAMS_FRESH: Duration = Duration::from_millis(150);
/// How often the settings of a plugin whose window is open are read, to see changes the plugin
/// does not report (some pick a preset without a word to the host).
const LOOK_EVERY: Duration = Duration::from_millis(500);

/// How to start the engine.
#[derive(Clone, Debug)]
pub struct TracktionOptions {
    /// Where the engine keeps its settings and what it learnt about plugins.
    pub settings_dir: PathBuf,
    /// Open the default audio device. Without one the engine renders but does not play.
    pub open_audio_device: bool,
    /// Host plugins (VST3, LV2, CLAP); without, plugin tracks are "not installed".
    pub enable_plugins: bool,
    /// The formats to scan for at startup, on the worker thread.
    pub scan: Vec<PluginFormat>,
}

impl TracktionOptions {
    /// No device, plugins on, every format scanned.
    pub fn new(settings_dir: impl Into<PathBuf>) -> Self {
        Self {
            settings_dir: settings_dir.into(),
            open_audio_device: false,
            enable_plugins: true,
            scan: PluginFormat::ALL.to_vec(),
        }
    }
}

/// What the worker leaves for the UI thread.
#[derive(Default)]
struct Shared {
    /// The latest view to bring the engine to, and whether a sync is on its way for it.
    view: Option<Project>,
    sync_queued: bool,
    catalog: Vec<PluginEntry>,
    scanning: bool,
    /// Per device of the last synced project.
    statuses: HashMap<DeviceId, DeviceStatus>,
    params: HashMap<DeviceId, Vec<ParamEntry>>,
    /// Numbers the sets of plugin parameters made here, in the order they were sent.
    sets_sent: u64,
    /// The number of the last set of each parameter. A reading of a device's parameters that
    /// was asked for before that set must not overwrite the value the set put in `params`: the
    /// reading would show the knob back where it was until the next one.
    last_set: HashMap<(DeviceId, String), u64>,
    /// States read from plugins, for the document.
    captured: Vec<((TrackId, DeviceId), SavedState)>,
    /// States read from plugins that the engine holds and the document does not show yet.
    /// Until it does, a view that still shows the state from before is not brought to the
    /// engine for that device: that would put the plugin back to its old settings for the
    /// moment between the read and the edit that records it.
    ahead: HashMap<DeviceId, Ahead>,
    /// State blobs the engine needs and does not have.
    wanted: HashSet<BlobHash>,
    /// Downloads blobs for the worker, straight to it; without, the session downloads them.
    fetcher: Option<crate::link::Fetcher>,
    /// To hand downloaded blobs to the worker from the network thread.
    jobs: Option<Sender<Job>>,
    /// Blobs being downloaded by the fetcher.
    fetching: HashSet<BlobHash>,
    notices: Vec<String>,
    waker: Option<Waker>,
}

enum Job {
    Sync,
    Scan(Vec<PluginFormat>),
    Play,
    Stop,
    Seek(i64),
    Loop(i64, i64),
    OutputGain(f32),
    SetParam {
        device: DeviceId,
        param: String,
        value: f32,
    },
    /// Reads a device's parameters; the number is `sets_sent` when it was asked for.
    Params(DeviceId, u64),
    Editor {
        device: DeviceId,
        show: bool,
    },
    Capture {
        track: TrackId,
        device: DeviceId,
    },
    /// Reads the settings of plugins whose windows are open, for changes not reported.
    Look(Vec<(TrackId, DeviceId)>),
    Blob(BlobHash, Arc<[u8]>),
    Retry {
        track: TrackId,
        device: DeviceId,
    },
    Render {
        path: PathBuf,
        from: i64,
        to: i64,
        reply: Sender<Result<(), String>>,
    },
    Flush(Sender<()>),
}

/// A device whose settings changed here and are to be read.
struct Pending {
    track: TrackId,
    first: Instant,
    last: Instant,
}

pub struct TracktionAudio {
    engine: Arc<Engine>,
    jobs: Option<Sender<Job>>,
    worker: Option<JoinHandle<()>>,
    shared: Arc<Mutex<Shared>>,
    enable_plugins: bool,
    playing: bool,
    /// A seek the engine may not have carried out yet: shown until it has.
    seek: Cell<Option<(i64, Instant)>>,
    output_gain: f32,
    /// Peaks since the UI last took them (reading the engine's state takes its peaks).
    peak: Cell<[f32; 2]>,
    /// The view last handed over, to see cheaply that nothing changed.
    given: Option<Project>,
    pending: HashMap<DeviceId, Pending>,
    editors: HashSet<DeviceId>,
    params_asked: HashMap<DeviceId, Instant>,
    looked_at: Instant,
    /// Devices looked at as if their windows were open (for tests; see [`TracktionAudio::watch`]).
    watched: HashSet<DeviceId>,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

fn raw_track(id: TrackId) -> u64 {
    id.0.0
}

fn raw_device(id: DeviceId) -> u64 {
    id.0.0
}

/// Whether two views are the same project for the engine, judged by shared structure.
/// A short `Vector` (the track order of a small project) is stored inline and never counts as
/// shared, so it is compared by value; it is a handful of ids.
fn same_view(a: &Project, b: &Project) -> bool {
    a.tracks.ptr_eq(&b.tracks)
        && a.tempo.ptr_eq(&b.tempo)
        && (a.track_order.ptr_eq(&b.track_order) || a.track_order == b.track_order)
        && a.master_gain == b.master_gain
}

/// The state a plugin device was found in, read here and not yet in the document.
struct Ahead {
    track: TrackId,
    /// What the read found.
    hash: BlobHash,
    /// What the document held when it was read.
    before: Option<BlobHash>,
    /// The session has taken the read: the next view it gives shows what it made of it.
    handed_over: bool,
}

/// A plugin device's state in a project, if it is a plugin device there.
fn state_in(project: &Project, track: TrackId, device: DeviceId) -> Option<Option<BlobHash>> {
    project
        .track(track)?
        .content
        .device(device)
        .and_then(|d| match &d.kind {
            DeviceKind::Plugin { state, .. } => Some(*state),
            DeviceKind::Builtin { .. } => None,
        })
}

/// The track a device is on.
fn track_of(project: &Project, device: DeviceId) -> Option<TrackId> {
    project
        .tracks
        .iter()
        .find(|(_, t)| t.content.device(device).is_some())
        .map(|(id, _)| *id)
}

fn plugin_devices(project: &Project) -> impl Iterator<Item = (TrackId, &mdaw_model::Device)> {
    project.tracks.iter().flat_map(|(id, t)| {
        t.content
            .devices
            .iter()
            .filter(|d| matches!(d.kind, DeviceKind::Plugin { .. }))
            .map(move |d| (*id, d))
    })
}

struct TeLive(Arc<Engine>);

impl LiveSink for TeLive {
    fn note_on(&self, track: TrackId, _key: u64, pitch: u8, vel: u8) {
        let _ = self
            .0
            .track_live_note(raw_track(track), pitch.min(127), vel.clamp(1, 127));
    }

    fn note_off(&self, track: TrackId, _key: u64, pitch: u8) {
        let _ = self.0.track_live_note(raw_track(track), pitch.min(127), 0);
    }
}

impl TracktionAudio {
    /// Starts the engine (and opens the device, if asked) and its worker. The plugin scan, if
    /// any, starts at once on the worker.
    pub fn new(options: TracktionOptions) -> Result<Self, String> {
        let mut config = Config::new("mdaw", &options.settings_dir);
        config.open_audio_device = options.open_audio_device;
        config.enable_plugins = options.enable_plugins;
        config.scanner_path = mdaw_tracktion::built_scanner_path().map(Path::to_path_buf);
        let engine = Arc::new(Engine::new(config).map_err(|e| e.to_string())?);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (jobs, received) = channel();
        let worker = Worker {
            engine: engine.clone(),
            shared: shared.clone(),
            synced: None,
            blobs: HashMap::new(),
            output_gain: 1.0,
            flush_path: options.settings_dir.join("flush.wav"),
            looks: HashMap::new(),
        };
        let worker = std::thread::Builder::new()
            .name("mdaw-engine-worker".into())
            .spawn(move || worker.run(received))
            .map_err(|e| e.to_string())?;
        if options.enable_plugins && !options.scan.is_empty() {
            lock(&shared).scanning = true;
            let _ = jobs.send(Job::Scan(options.scan.clone()));
        }
        Ok(Self {
            engine,
            jobs: Some(jobs),
            worker: Some(worker),
            shared,
            enable_plugins: options.enable_plugins,
            playing: false,
            seek: Cell::new(None),
            output_gain: 1.0,
            peak: Cell::new([0.0; 2]),
            given: None,
            pending: HashMap::new(),
            editors: HashSet::new(),
            looked_at: Instant::now(),
            watched: HashSet::new(),
            params_asked: HashMap::new(),
        })
    }

    /// The engine, for what this type does not offer (tests, diagnostics). Calls on it wait
    /// for the engine's thread.
    /// Looks at a device's settings as if its window were open, for tests of plugins that
    /// change without a report (the test synth has no window).
    #[doc(hidden)]
    pub fn watch(&mut self, device: DeviceId) {
        self.watched.insert(device);
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    fn send(&self, job: Job) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(job);
        }
    }

    /// Waits until the worker has done everything asked of it so far.
    pub fn wait_idle(&self) {
        let (tx, rx) = channel();
        self.send(Job::Flush(tx));
        let _ = rx.recv();
    }

    /// The engine's state, read now; its peaks are kept for [`Audio::take_peak`].
    fn read(&self) -> EngineState {
        let state = self.engine.state();
        let [l, r] = self.peak.get();
        self.peak.set([l.max(state.peak_left), r.max(state.peak_right)]);
        state
    }

    /// Marks a device whose settings changed here, to be read and put into the document.
    fn changed(&mut self, track: TrackId, device: DeviceId) {
        let now = Instant::now();
        self.pending
            .entry(device)
            .and_modify(|p| p.last = now)
            .or_insert(Pending {
                track,
                first: now,
                last: now,
            });
    }

    /// Asks for the settings of the changed devices that are due (all of them with `all`).
    fn capture_due(&mut self, all: bool) {
        let now = Instant::now();
        let due: Vec<DeviceId> = self
            .pending
            .iter()
            .filter(|(_, p)| {
                all || now.duration_since(p.last) >= CAPTURE_QUIET
                    || now.duration_since(p.first) >= CAPTURE_LATEST
            })
            .map(|(id, _)| *id)
            .collect();
        for device in due {
            if let Some(p) = self.pending.remove(&device) {
                crate::trace::step(|| {
                    format!(
                        "reads the settings, {} ms after the first change and {} ms after the last",
                        p.first.elapsed().as_millis(),
                        p.last.elapsed().as_millis()
                    )
                });
                self.send(Job::Capture {
                    track: p.track,
                    device,
                });
            }
        }
    }

    fn status_of(
        &self,
        shared: &Shared,
        project: &Project,
        track: TrackId,
        device: DeviceId,
    ) -> DeviceStatus {
        let Some(d) = project.track(track).and_then(|t| t.content.device(device)) else {
            return DeviceStatus::Loading;
        };
        if matches!(d.kind, DeviceKind::Builtin { .. }) {
            return DeviceStatus::Builtin;
        }
        if !self.enable_plugins {
            return DeviceStatus::NotInstalled;
        }
        match shared.statuses.get(&device) {
            None => DeviceStatus::Loading,
            Some(DeviceStatus::NotInstalled) if shared.scanning => DeviceStatus::Loading,
            Some(status) => status.clone(),
        }
    }

    /// A word for the user when the audio stopped: the plugins that may be to blame.
    fn stalled(&self, project: &Project) -> String {
        let shared = lock(&self.shared);
        let mut names = Vec::new();
        for (track, device) in plugin_devices(project) {
            if let DeviceKind::Plugin { format, id, .. } = &device.kind {
                let name = shared
                    .catalog
                    .iter()
                    .find(|p| p.format == *format && p.id == *id)
                    .map_or(id.clone(), |p| p.name.clone());
                let track = project
                    .track(track)
                    .map(|t| t.shell.name.clone())
                    .unwrap_or_default();
                names.push(format!("{name} on {track}"));
            }
        }
        if names.is_empty() {
            "The audio stopped: the audio device does not answer.".into()
        } else {
            format!(
                "The audio stopped: a plugin may hang. Plugins loaded: {}. Save your work; restarting mdaw without that plugin may be needed.",
                names.join(", ")
            )
        }
    }
}

impl Drop for TracktionAudio {
    fn drop(&mut self) {
        // The worker ends when its queue is gone, the handle the downloads use included; the
        // engine goes with the last holder (live notes may hold it a moment longer). A download
        // still on its way holds a handle of its own until it ends, which the link's end makes
        // happen.
        {
            let mut shared = lock(&self.shared);
            shared.jobs = None;
            shared.fetcher = None;
        }
        self.jobs = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl From<TracktionAudio> for Box<dyn Audio> {
    fn from(audio: TracktionAudio) -> Self {
        Box::new(audio)
    }
}

impl Audio for TracktionAudio {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn set_waker(&mut self, waker: Waker) {
        lock(&self.shared).waker = Some(waker);
    }

    fn set_project(&mut self, project: &Project) {
        if self.given.as_ref().is_some_and(|given| same_view(given, project)) {
            return;
        }
        self.given = Some(project.clone());
        let queue = {
            let mut shared = lock(&self.shared);
            // The session has seen these reads and made its edits: from this view on, what the
            // document shows is what counts, the old state included if it refused the edit.
            shared.ahead.retain(|device, ahead| {
                let now = state_in(project, ahead.track, *device);
                !ahead.handed_over && now == Some(ahead.before)
            });
            shared.view = Some(project.clone());
            !std::mem::replace(&mut shared.sync_queued, true)
        };
        if queue {
            self.send(Job::Sync);
        }
    }

    fn frame(&mut self, project: &Project) -> Vec<String> {
        let state = self.read();
        if let Some((tick, at)) = self.seek.get()
            && (state.position_tick == tick || at.elapsed() > Duration::from_secs(1))
        {
            self.seek.set(None);
        }
        let mut notices = Vec::new();
        while let Some(event) = self.engine.poll_event() {
            match event {
                Event::ParamChanged { device, .. } | Event::StateChanged { device } => {
                    let device = DeviceId(mdaw_model::EntityId(device));
                    if let Some(track) = track_of(project, device) {
                        if !self.pending.contains_key(&device) {
                            crate::trace::step(|| match &event {
                                Event::ParamChanged { param_id, .. } => {
                                    format!("plugin reports a change of parameter {param_id}")
                                }
                                _ => "plugin reports its settings changed".into(),
                            });
                        }
                        self.changed(track, device);
                    }
                }
                Event::EditorClosed { device } => {
                    self.editors.remove(&DeviceId(mdaw_model::EntityId(device)));
                }
                Event::AudioStalled => notices.push(self.stalled(project)),
                Event::DeviceChanged => {}
            }
        }
        self.capture_due(false);
        if !(self.editors.is_empty() && self.watched.is_empty()) && self.looked_at.elapsed() >= LOOK_EVERY {
            self.looked_at = Instant::now();
            let open: Vec<(TrackId, DeviceId)> = self
                .editors
                .union(&self.watched)
                .filter(|d| !self.pending.contains_key(d))
                .filter_map(|d| Some((track_of(project, *d)?, *d)))
                .collect();
            if !open.is_empty() {
                self.send(Job::Look(open));
            }
        }
        notices.append(&mut lock(&self.shared).notices);
        notices
    }

    fn next_frame_in(&self) -> Option<Duration> {
        let busy = {
            let shared = lock(&self.shared);
            shared.scanning || shared.sync_queued || !shared.captured.is_empty()
        };
        if !self.pending.is_empty() || busy {
            Some(Duration::from_millis(100))
        } else if !self.editors.is_empty() || !self.watched.is_empty() {
            // Changes made in a plugin's window come as events, or are found by looking.
            Some(Duration::from_millis(50))
        } else {
            None
        }
    }

    fn play(&mut self) {
        self.playing = true;
        self.send(Job::Play);
    }

    fn stop(&mut self) {
        self.playing = false;
        self.send(Job::Stop);
    }

    fn seek(&mut self, tick: Tick) {
        let tick = tick.0.max(0);
        self.seek.set(Some((tick, Instant::now())));
        self.send(Job::Seek(tick));
    }

    fn set_loop(&mut self, range: Option<TickRange>) {
        let (start, end) = range.map_or((0, 0), |r| (r.start.0.max(0), r.end.0.max(0)));
        self.send(Job::Loop(start, end));
    }

    fn is_playing(&self) -> bool {
        self.playing
    }

    fn position(&self) -> f64 {
        match self.seek.get() {
            Some((tick, _)) => tick as f64,
            None => self.read().position_tick as f64,
        }
    }

    fn position_heard(&self) -> (f64, i64) {
        let now = clock::now_micros();
        if let Some((tick, _)) = self.seek.get() {
            return (tick as f64, now);
        }
        let state = self.read();
        let shift = (state.output_latency_seconds - state.position_age_seconds) * 1e6;
        (state.position_tick as f64, now + shift as i64)
    }

    fn live(&self) -> Arc<dyn LiveSink> {
        Arc::new(TeLive(self.engine.clone()))
    }

    fn set_output_gain(&mut self, gain: f32) {
        let gain = if gain.is_finite() {
            gain.clamp(0.0, 4.0)
        } else {
            1.0
        };
        self.output_gain = gain;
        self.send(Job::OutputGain(gain));
    }

    fn output_gain(&self) -> f32 {
        self.output_gain
    }

    fn take_peak(&mut self) -> [f32; 2] {
        self.read();
        self.peak.replace([0.0; 2])
    }

    fn description(&self) -> String {
        let state = self.read();
        if !state.device_open {
            return "Tracktion, no audio device".into();
        }
        format!(
            "Tracktion, {:.0} Hz, {} frames, {:.0} ms, {} xruns, {:.0}% CPU",
            state.sample_rate,
            state.block_size,
            state.output_latency_seconds * 1000.0,
            state.xruns,
            state.cpu * 100.0
        )
    }

    fn plugins_enabled(&self) -> bool {
        self.enable_plugins
    }

    fn scanning_plugins(&self) -> bool {
        lock(&self.shared).scanning
    }

    fn plugin_catalog(&self) -> Vec<PluginEntry> {
        lock(&self.shared).catalog.clone()
    }

    fn installed(&self, format: PluginFormat, id: &str) -> Option<PluginEntry> {
        lock(&self.shared)
            .catalog
            .iter()
            .find(|p| p.format == format && p.id == id)
            .cloned()
    }

    fn device_status(&self, project: &Project, track: TrackId, device: DeviceId) -> DeviceStatus {
        let shared = lock(&self.shared);
        self.status_of(&shared, project, track, device)
    }

    fn track_sound(&self, project: &Project, track: TrackId) -> TrackSound {
        let Some(t) = project.track(track) else {
            return TrackSound::Silent;
        };
        let shared = lock(&self.shared);
        track_sound_from(
            t.content
                .devices
                .iter()
                .map(|d| self.status_of(&shared, project, track, d.id)),
        )
    }

    fn plugin_params(
        &mut self,
        _track: TrackId,
        device: DeviceId,
        limit: usize,
    ) -> Option<(Vec<ParamEntry>, usize)> {
        let stale = self
            .params_asked
            .get(&device)
            .is_none_or(|at| at.elapsed() >= PARAMS_FRESH);
        if stale {
            self.params_asked.insert(device, Instant::now());
            let asked_after = lock(&self.shared).sets_sent;
            self.send(Job::Params(device, asked_after));
        }
        let shared = lock(&self.shared);
        let params = shared.params.get(&device)?;
        Some((params.iter().take(limit).cloned().collect(), params.len()))
    }

    fn set_plugin_param(&mut self, track: TrackId, device: DeviceId, param: &str, value: f64) {
        let normalised = {
            let mut shared = lock(&self.shared);
            let Some(entry) = shared
                .params
                .get_mut(&device)
                .and_then(|list| list.iter_mut().find(|p| p.id == param))
            else {
                return;
            };
            entry.value = value.clamp(entry.min, entry.max);
            let span = entry.max - entry.min;
            let normalised = if span > 0.0 {
                (entry.value - entry.min) / span
            } else {
                0.0
            };
            shared.sets_sent += 1;
            let number = shared.sets_sent;
            shared.last_set.insert((device, param.to_string()), number);
            normalised
        };
        self.send(Job::SetParam {
            device,
            param: param.to_string(),
            value: normalised as f32,
        });
        // The bridge does not report the application's own sets; the settings changed all the
        // same, as when the knob is turned in the plugin's window.
        self.changed(track, device);
    }

    fn has_editor(&self, track: TrackId, device: DeviceId) -> bool {
        // Whether the plugin has a window is found out when it is asked to open one.
        let plugin = self
            .given
            .as_ref()
            .and_then(|p| p.track(track))
            .and_then(|t| t.content.device(device))
            .is_some_and(|d| matches!(d.kind, DeviceKind::Plugin { .. }));
        plugin && lock(&self.shared).statuses.get(&device) == Some(&DeviceStatus::Running)
    }

    fn editor_open(&self, _track: TrackId, device: DeviceId) -> bool {
        self.editors.contains(&device)
    }

    fn toggle_editor(&mut self, _track: TrackId, device: DeviceId, _title: &str) -> Result<(), String> {
        let show = !self.editors.contains(&device);
        if show {
            self.editors.insert(device);
        } else {
            self.editors.remove(&device);
        }
        self.send(Job::Editor { device, show });
        Ok(())
    }

    fn restart_plugin(&mut self, track: TrackId, device: DeviceId) {
        lock(&self.shared).statuses.insert(device, DeviceStatus::Loading);
        self.send(Job::Retry { track, device });
    }

    fn set_fetcher(&mut self, fetcher: crate::link::Fetcher) {
        let mut shared = lock(&self.shared);
        shared.fetcher = Some(fetcher);
        shared.jobs = self.jobs.clone();
    }

    fn wanted_blobs(&mut self, asked: &HashSet<BlobHash>) -> Vec<BlobHash> {
        let shared = lock(&self.shared);
        if shared.fetcher.is_some() {
            return Vec::new();
        }
        shared.wanted.difference(asked).copied().collect()
    }

    fn blob_arrived(&mut self, hash: BlobHash, bytes: Option<Arc<[u8]>>) {
        // No longer wanted from here on, so that the next frame does not ask for it again
        // while the worker has yet to take it.
        lock(&self.shared).wanted.remove(&hash);
        if let Some(bytes) = bytes {
            self.send(Job::Blob(hash, bytes));
        }
    }

    fn changed_states(&mut self, project: &Project, now: bool) -> Vec<((TrackId, DeviceId), SavedState)> {
        if now {
            self.capture_due(true);
            self.wait_idle();
        }
        let captured = {
            let mut shared = lock(&self.shared);
            let captured = std::mem::take(&mut shared.captured);
            for ((_, device), (hash, _)) in &captured {
                if let Some(ahead) = shared.ahead.get_mut(device)
                    && ahead.hash == *hash
                {
                    ahead.handed_over = true;
                }
            }
            captured
        };
        captured
            .into_iter()
            .filter(|((track, device), (hash, _))| {
                let held = project
                    .track(*track)
                    .and_then(|t| t.content.device(*device))
                    .map(|d| match &d.kind {
                        DeviceKind::Plugin { state, .. } => *state == Some(*hash),
                        DeviceKind::Builtin { .. } => true,
                    });
                held == Some(false)
            })
            .collect()
    }

    fn render(&mut self, path: &Path, from: Tick, to: Tick) -> Result<(), String> {
        let (reply, answer) = channel();
        self.send(Job::Render {
            path: path.to_path_buf(),
            from: from.0,
            to: to.0,
            reply,
        });
        answer
            .recv()
            .unwrap_or_else(|_| Err("the engine's worker is gone".into()))
    }
}

// ---- the worker ----------------------------------------------------------------------------------

struct Worker {
    engine: Arc<Engine>,
    shared: Arc<Mutex<Shared>>,
    /// The project the engine holds; `None` until the first load.
    synced: Option<Project>,
    /// Plugin states at hand: downloaded, or read from the plugins here.
    blobs: HashMap<BlobHash, Arc<[u8]>>,
    output_gain: f32,
    /// Where the short render goes that lets the plugins take parameters without a device.
    flush_path: PathBuf,
    /// What reading the settings of a plugin with an open window last gave, and a different
    /// reading seen once since: a change counts when two readings in a row agree on it, so a
    /// plugin that writes its settings a little differently each time sends nothing.
    looks: HashMap<DeviceId, (BlobHash, Option<BlobHash>)>,
}

impl Worker {
    fn run(mut self, jobs: Receiver<Job>) {
        while let Ok(job) = jobs.recv() {
            self.job(job);
            let waker = lock(&self.shared).waker.clone();
            if let Some(waker) = waker {
                waker();
            }
        }
    }

    fn notice(&self, text: String) {
        lock(&self.shared).notices.push(text);
    }

    fn failed(&self, what: &str, result: Result<(), mdaw_tracktion::Error>) {
        if let Err(e) = result {
            self.notice(format!("The audio engine could not {what}: {}", e.message));
        }
    }

    fn job(&mut self, job: Job) {
        let engine = self.engine.clone();
        match job {
            Job::Sync => self.sync(),
            Job::Scan(formats) => self.scan(&formats),
            Job::Play => self.failed("play", engine.transport_play()),
            Job::Stop => self.failed("stop", engine.transport_stop()),
            Job::Seek(tick) => self.failed("move the playhead", engine.transport_seek(tick)),
            Job::Loop(start, end) => self.failed("set the loop", engine.transport_set_loop(start, end)),
            Job::OutputGain(gain) => {
                self.output_gain = gain;
                self.master_gain();
            }
            Job::SetParam { device, param, value } => {
                self.failed(
                    "set the parameter",
                    engine.device_set_param(raw_device(device), &param, value.clamp(0.0, 1.0)),
                );
            }
            Job::Params(device, asked_after) => self.read_params(device, asked_after),
            Job::Editor { device, show } => {
                self.failed(
                    "open the plugin's window",
                    engine.device_show_editor(raw_device(device), show),
                );
            }
            Job::Capture { track, device } => self.capture(track, device),
            Job::Look(open) => self.look(&open),
            Job::Blob(hash, bytes) => self.blob(hash, bytes),
            Job::Retry { track, device } => {
                self.retry(track, device);
                self.publish_statuses(&[]);
            }
            Job::Render {
                path,
                from,
                to,
                reply,
            } => {
                let _ = reply.send(self.render(&path, from, to));
            }
            Job::Flush(reply) => {
                let _ = reply.send(());
            }
        }
    }

    /// Reads a device's parameters for the device tab; `asked_after` is `sets_sent` when the
    /// reading was asked for.
    fn read_params(&self, device: DeviceId, asked_after: u64) {
        let params = self.engine.device_params(raw_device(device)).unwrap_or_default();
        let mut list: Vec<ParamEntry> = params
            .into_iter()
            .map(|p| ParamEntry {
                id: p.id,
                name: p.name,
                value: p.min + p.value * (p.max - p.min),
                min: p.min,
                max: p.max,
                stepped: p.steps > 0,
                text: p.text,
            })
            .collect();
        let mut shared = lock(&self.shared);
        // A parameter set here after this reading was asked for may not have reached the
        // plugin when it was taken: keep the value the set showed.
        if let Some(shown) = shared.params.get(&device) {
            for entry in &mut list {
                let newer = shared
                    .last_set
                    .get(&(device, entry.id.clone()))
                    .is_some_and(|number| *number > asked_after);
                if newer && let Some(old) = shown.iter().find(|p| p.id == entry.id) {
                    entry.value = old.value;
                    entry.text = old.text.clone();
                }
            }
        }
        shared.params.insert(device, list);
    }

    /// After new settings went into plugins: the device tab shows their parameters from what
    /// was read before, and asks again only when it redraws, which nothing may make it do.
    /// So the readings of those devices that are shown are taken again now; the window is woken
    /// when this job is done.
    fn reread_params(&self, devices: &[DeviceId]) {
        let (shown, asked_after): (Vec<DeviceId>, u64) = {
            let shared = lock(&self.shared);
            (
                devices
                    .iter()
                    .copied()
                    .filter(|d| shared.params.contains_key(d))
                    .collect(),
                shared.sets_sent,
            )
        };
        for device in shown {
            self.read_params(device, asked_after);
        }
    }

    fn resolver(&self) -> impl Fn(&BlobHash) -> Option<Vec<u8>> + '_ {
        |hash| self.blobs.get(hash).map(|b| b.to_vec())
    }

    fn master_gain(&self) {
        let gain = self.synced.as_ref().map_or(1.0, |p| p.master_gain) * self.output_gain;
        if let Err(e) = self.engine.master_set_gain(gain) {
            self.notice(format!(
                "The audio engine could not set the volume: {}",
                e.message
            ));
        }
    }

    fn sync(&mut self) {
        let view = {
            let mut shared = lock(&self.shared);
            shared.sync_queued = false;
            let ahead: Vec<(TrackId, DeviceId, BlobHash, Option<BlobHash>)> = shared
                .ahead
                .iter()
                .map(|(device, a)| (a.track, *device, a.hash, a.before))
                .collect();
            shared.view.clone().map(|mut view| {
                for (track, device, hash, before) in ahead {
                    // Only while the document still shows the state from before the read; a
                    // state that came from elsewhere since wins.
                    if state_in(&view, track, device) == Some(before) {
                        set_state(&mut view, track, device, hash);
                    }
                }
                view
            })
        };
        let Some(view) = view else { return };
        let restated: Vec<DeviceId> = plugin_devices(&view)
            .filter(|(track, d)| {
                let now = state_in(&view, *track, d.id);
                self.synced.as_ref().map(|p| state_in(p, *track, d.id)) != Some(now)
            })
            .map(|(_, d)| d.id)
            .collect();
        let engine = self.engine.clone();
        let result = {
            let blobs = self.resolver();
            match &self.synced {
                Some(before) => mdaw_tracktion::sync_project(&engine, before, &view, &blobs),
                None => mdaw_tracktion::load_project(&engine, &view, &blobs),
            }
        };
        let problems = match result {
            Ok(problems) => problems,
            Err(e) => {
                // The diff and the engine no longer agree: start over from the view.
                self.notice(format!(
                    "The audio engine is reloading the project ({}).",
                    e.message
                ));
                let blobs = self.resolver();
                match mdaw_tracktion::load_project(&engine, &view, &blobs) {
                    Ok(problems) => problems,
                    Err(e) => {
                        self.notice(format!(
                            "The audio engine could not load the project: {}",
                            e.message
                        ));
                        Vec::new()
                    }
                }
            }
        };
        let gain_changed = self
            .synced
            .as_ref()
            .is_none_or(|p| p.master_gain != view.master_gain);
        if !restated.is_empty() {
            crate::trace::step(|| format!("engine took new settings for {} plugins", restated.len()));
        }
        for device in &restated {
            self.looks.remove(device);
        }
        self.synced = Some(view);
        self.reread_params(&restated);
        if gain_changed || self.output_gain != 1.0 {
            self.master_gain();
        }
        self.publish_statuses(&problems);
    }

    /// Works out how each plugin device of the synced project is doing.
    fn publish_statuses(&self, problems: &[Problem]) {
        let Some(project) = &self.synced else { return };
        let mut wanted = Vec::new();
        for problem in problems {
            match problem {
                Problem::StateNotApplied { blob, reason, .. } if reason.contains("not at hand") => {
                    wanted.push(*blob)
                }
                Problem::StateNotApplied { reason, .. } => {
                    self.notice(format!("A plugin did not take its settings: {reason}"))
                }
                _ => {}
            }
        }
        let mut shared = lock(&self.shared);
        shared.wanted.extend(wanted.iter().copied());
        // With a fetcher the download starts here, on the worker, and comes back to it: no
        // round trip through the window's thread, which may run seldom while it is not shown.
        if let (Some(fetcher), Some(jobs)) = (shared.fetcher.clone(), shared.jobs.clone()) {
            for hash in wanted {
                if !shared.fetching.insert(hash) {
                    continue;
                }
                crate::trace::step(|| format!("engine asks for settings {}", crate::trace::short(&hash)));
                let (jobs, back) = (jobs.clone(), self.shared.clone());
                fetcher.fetch(
                    hash,
                    Box::new(move |bytes| {
                        crate::trace::step(|| {
                            format!("engine's download {} arrived", crate::trace::short(&hash))
                        });
                        match bytes {
                            Some(bytes) => {
                                let _ = jobs.send(Job::Blob(hash, bytes));
                            }
                            None => {
                                let mut shared = lock(&back);
                                shared.wanted.remove(&hash);
                                shared.fetching.remove(&hash);
                            }
                        }
                    }),
                );
            }
        }
        let mut statuses = HashMap::new();
        for (_, device) in plugin_devices(project) {
            let DeviceKind::Plugin { format, id, .. } = &device.kind else {
                continue;
            };
            let status = if self.engine.has(EntityKind::Device, raw_device(device.id)) {
                DeviceStatus::Running
            } else {
                let skipped = problems.iter().find_map(|p| match p {
                    Problem::DeviceSkipped {
                        device: d, reason, ..
                    } if *d == device.id => Some(reason.clone()),
                    _ => None,
                });
                let installed = shared.catalog.iter().any(|p| p.format == *format && p.id == *id);
                match (skipped, shared.statuses.get(&device.id)) {
                    (Some(reason), _) if installed => DeviceStatus::Failed(reason),
                    (Some(_), _) => DeviceStatus::NotInstalled,
                    (None, Some(DeviceStatus::Failed(reason))) => DeviceStatus::Failed(reason.clone()),
                    (None, _) if installed => DeviceStatus::Failed("not loaded".into()),
                    (None, _) => DeviceStatus::NotInstalled,
                }
            };
            statuses.insert(device.id, status);
        }
        shared.statuses = statuses;
    }

    fn scan(&mut self, formats: &[PluginFormat]) {
        for format in formats {
            if let Err(e) = self.engine.plugins_scan(format.name()) {
                self.notice(format!(
                    "Looking for {} plugins failed: {}",
                    format.name(),
                    e.message
                ));
            }
        }
        let list = self.engine.plugins().unwrap_or_default();
        let catalog: Vec<PluginEntry> = list
            .into_iter()
            .filter_map(|p| {
                Some(PluginEntry {
                    format: PluginFormat::from_name(&p.format)?,
                    id: p.identifier,
                    name: p.name,
                    vendor: p.vendor,
                    instrument: p.instrument,
                    location: String::new(),
                })
            })
            .collect();
        {
            let mut shared = lock(&self.shared);
            shared.catalog = catalog;
            shared.scanning = false;
        }
        // Devices left out because their plugin was not known yet.
        let missing: Vec<(TrackId, DeviceId)> = self
            .synced
            .as_ref()
            .map(|p| {
                plugin_devices(p)
                    .filter(|(_, d)| !self.engine.has(EntityKind::Device, raw_device(d.id)))
                    .map(|(t, d)| (t, d.id))
                    .collect()
            })
            .unwrap_or_default();
        let mut problems = Vec::new();
        for (track, device) in missing {
            problems.extend(self.retry(track, device));
        }
        self.publish_statuses(&problems);
    }

    /// Tries to put a device that is not in the engine into it again.
    fn retry(&self, track: TrackId, device: DeviceId) -> Vec<Problem> {
        let Some(project) = &self.synced else {
            return Vec::new();
        };
        let Some(d) = project
            .track(track)
            .and_then(|t| t.content.device(device))
            .cloned()
        else {
            return Vec::new();
        };
        if self.engine.has(EntityKind::Device, raw_device(device)) {
            return Vec::new();
        }
        let op = Op::DeviceAdd {
            track,
            device: d,
            after: None,
        };
        let blobs = self.resolver();
        match mdaw_tracktion::apply_op(&self.engine, project, &op, project, &blobs) {
            Ok(problems) => problems,
            Err(e) => {
                self.notice(format!("The plugin could not be started: {}", e.message));
                Vec::new()
            }
        }
    }

    /// Reads the settings of plugins whose windows are open; one that changed without a report
    /// is read as if it had reported.
    fn look(&mut self, open: &[(TrackId, DeviceId)]) {
        for &(track, device) in open {
            let raw = raw_device(device);
            if !self.engine.has(EntityKind::Device, raw) {
                continue;
            }
            let bytes = self.engine.device_get_state(raw);
            if bytes.is_empty() {
                continue;
            }
            let hash = mdaw_proto::blob_hash(&bytes);
            match self.looks.get(&device).copied() {
                None => {
                    self.looks.insert(device, (hash, None));
                }
                Some((last, _)) if last == hash => {
                    self.looks.insert(device, (last, None));
                }
                Some((last, Some(seen))) if seen == hash => {
                    crate::trace::step(|| {
                        format!(
                            "found the settings changed without a report: {} -> {}",
                            crate::trace::short(&last),
                            crate::trace::short(&hash)
                        )
                    });
                    self.capture(track, device);
                }
                Some((last, _)) => {
                    self.looks.insert(device, (last, Some(hash)));
                }
            }
        }
    }

    fn capture(&mut self, track: TrackId, device: DeviceId) {
        let Some(project) = &self.synced else { return };
        let is_plugin = project
            .track(track)
            .and_then(|t| t.content.device(device))
            .is_some_and(|d| matches!(d.kind, DeviceKind::Plugin { .. }));
        let raw = raw_device(device);
        if !is_plugin || !self.engine.has(EntityKind::Device, raw) {
            return;
        }
        // A parameter reaches the plugin with the next block it processes. With a device open
        // blocks are processed all the time; without, a moment of render does it.
        if !self.engine.state().device_open
            && let Err(e) = self.engine.render(&self.flush_path, 0, 1, RENDER_RATE)
        {
            self.notice(format!("Plugin settings may be read too early: {}", e.message));
        }
        let bytes: Arc<[u8]> = self.engine.device_get_state(raw).into();
        if bytes.is_empty() {
            return;
        }
        let hash = mdaw_proto::blob_hash(&bytes);
        crate::trace::step(|| {
            format!(
                "settings read: {} ({} bytes)",
                crate::trace::short(&hash),
                bytes.len()
            )
        });
        self.looks.insert(device, (hash, None));
        self.blobs.insert(hash, bytes.clone());
        // The engine holds this state now: the edit that brings it into the document changes
        // nothing here when it comes back through the view, and until then a view showing the
        // old state does not take it back (see `Shared::ahead`).
        let before = self
            .synced
            .as_ref()
            .and_then(|p| state_in(p, track, device))
            .flatten();
        if let Some(synced) = &mut self.synced {
            set_state(synced, track, device, hash);
        }
        let mut shared = lock(&self.shared);
        shared.ahead.insert(
            device,
            Ahead {
                track,
                hash,
                before,
                handed_over: false,
            },
        );
        shared.captured.push(((track, device), (hash, bytes)));
    }

    fn blob(&mut self, hash: BlobHash, bytes: Arc<[u8]>) {
        self.blobs.insert(hash, bytes);
        {
            let mut shared = lock(&self.shared);
            shared.wanted.remove(&hash);
            shared.fetching.remove(&hash);
        }
        let Some(project) = &self.synced else { return };
        let waiting: Vec<(TrackId, DeviceId)> = plugin_devices(project)
            .filter(|(_, d)| matches!(&d.kind, DeviceKind::Plugin { state: Some(s), .. } if *s == hash))
            .map(|(t, d)| (t, d.id))
            .collect();
        let blobs = self.resolver();
        let mut problems = Vec::new();
        for &(track, id) in &waiting {
            let op = Op::DeviceSetState {
                track,
                id,
                state: Some(hash),
            };
            match mdaw_tracktion::apply_op(&self.engine, project, &op, project, &blobs) {
                Ok(found) => problems.extend(found),
                Err(e) => self.notice(format!("A plugin did not take its settings: {}", e.message)),
            }
        }
        drop(blobs);
        let applied: Vec<DeviceId> = waiting.iter().map(|(_, id)| *id).collect();
        crate::trace::step(|| {
            format!(
                "engine took downloaded settings {} for {} plugins",
                crate::trace::short(&hash),
                applied.len()
            )
        });
        for device in &applied {
            self.looks.remove(device);
        }
        self.reread_params(&applied);
        self.publish_statuses(&problems);
    }

    fn render(&mut self, path: &Path, from: i64, to: i64) -> Result<(), String> {
        // The listener's own volume is not part of a render.
        let own = self.output_gain;
        if own != 1.0 {
            self.output_gain = 1.0;
            self.master_gain();
        }
        let result = self
            .engine
            .render(path, from, to, RENDER_RATE)
            .map_err(|e| e.message);
        if own != 1.0 {
            self.output_gain = own;
            self.master_gain();
        }
        result
    }
}

/// Sets a plugin device's state in a project, as if by `DeviceSetState`.
fn set_state(project: &mut Project, track: TrackId, device: DeviceId, hash: BlobHash) {
    let Some(t) = project.tracks.get_mut(&track) else {
        return;
    };
    let t = Arc::make_mut(t);
    let content: &mut TrackContent = Arc::make_mut(&mut t.content);
    for d in content.devices.iter_mut() {
        if d.id == device
            && let DeviceKind::Plugin { state, .. } = &mut d.kind
        {
            *state = Some(hash);
        }
    }
}
