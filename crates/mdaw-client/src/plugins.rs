//! Plugins on this client: which are installed, the running instances of the document's plugin
//! devices, and their state blobs.
//!
//! Each client decides for itself how a track with plugins is heard: with every plugin of the
//! track installed and its state at hand it runs them; otherwise the track is silent here.
//! Everything here speaks to `mdaw_plugin`'s format-neutral interface only.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

use mdaw_dsp::{ExternalDevice, MAX_BLOCK, PluginRef, PluginSource};
use mdaw_model::{BlobHash, DeviceId, DeviceKind, PluginFormat, Project, TrackId};
use mdaw_plugin::{Host, Instance, ParamInfo, PluginInfo, Settings, Waker};

pub use crate::audio::{SavedState, TrackSound};

/// An instance made on another thread, its settings loaded.
struct Made {
    instance: Box<dyn Instance + Send>,
    baseline: Option<BlobHash>,
}

/// An instance being made on another thread, for this state of the document.
struct Starting {
    state: Option<BlobHash>,
    /// The plugin's name, for what is said if it fails.
    name: String,
    result: Receiver<Result<Made, String>>,
}

/// How long parameter values read for the panel are good for.
const PARAMS_FRESH: Duration = Duration::from_millis(100);

/// How often the settings of plugins are looked at, to put those that changed into the
/// document.
pub const SAVE_EVERY: Duration = Duration::from_secs(2);
/// How often the main loop runs at least while a plugin's own window is open: plugins draw from
/// their timers, which run only then.
pub const WINDOW_PACE: Duration = Duration::from_millis(10);
/// How often it runs at least while plugins run without a window, for the callbacks they ask for.
pub const PACE: Duration = Duration::from_millis(50);

struct Running {
    instance: Box<dyn Instance>,
    /// The state the instance was made from or last saved to: what the document says, unless
    /// the document has moved on.
    state: Option<BlobHash>,
    /// What the instance's own save gave right after it was made, or when it was last saved:
    /// a save that gives something else means its settings changed. Plugins do not always say
    /// so themselves (Vital does not for its presets), and the same instance saves the same
    /// settings the same way.
    baseline: Option<BlobHash>,
}

pub struct Plugins {
    host: Option<Host>,
    sample_rate: u32,
    running: HashMap<(TrackId, DeviceId), Running>,
    states: HashMap<BlobHash, Arc<[u8]>>,
    /// State blobs asked for that are not here.
    wanted: HashSet<BlobHash>,
    /// Something became available since the graph was built: build it again.
    dirty: bool,
    /// Devices whose plugin crashed or failed, with the plugin's name, until they are
    /// restarted. Their tracks are silent.
    crashed: HashMap<(TrackId, DeviceId), String>,
    /// Failures to tell the user about at the next `idle`.
    failures: Vec<((TrackId, DeviceId), String)>,
    /// Instances being made on other threads, and those made, waiting for the graph.
    starting: HashMap<(TrackId, DeviceId), Starting>,
    ready: HashMap<(TrackId, DeviceId), (Option<BlobHash>, Made)>,
    /// Wakes the main loop from other threads.
    waker: Option<Waker>,
    /// Parameters as last read, for the panel.
    params: HashMap<(TrackId, DeviceId), (Instant, Vec<ParamInfo>, usize)>,
}

impl Plugins {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            host: None,
            sample_rate,
            running: HashMap::new(),
            states: HashMap::new(),
            wanted: HashSet::new(),
            dirty: false,
            crashed: HashMap::new(),
            failures: Vec::new(),
            starting: HashMap::new(),
            ready: HashMap::new(),
            waker: None,
            params: HashMap::new(),
        }
    }

    /// Who to wake when a plugin has something for the main loop.
    pub fn set_waker(&mut self, waker: Waker) {
        if let Some(host) = self.host.as_mut() {
            host.set_waker(waker.clone());
        }
        self.waker = Some(waker);
    }

    pub fn set_host(&mut self, host: Option<Host>) {
        self.close_all();
        self.host = host;
        if let (Some(host), Some(waker)) = (self.host.as_mut(), self.waker.clone()) {
            host.set_waker(waker);
        }
        self.dirty = true;
    }

    pub fn has_host(&self) -> bool {
        self.host.is_some()
    }

    pub fn catalog(&self) -> &[PluginInfo] {
        self.host.as_ref().map_or(&[], |h| h.catalog())
    }

    pub fn installed(&self, format: PluginFormat, id: &str) -> Option<&PluginInfo> {
        self.host.as_ref()?.find(format, id)
    }

    /// Blobs wanted and not asked for yet. Each is given out once.
    pub fn take_wanted(&mut self, asked: &HashSet<BlobHash>) -> Vec<BlobHash> {
        self.wanted.difference(asked).copied().collect()
    }

    pub fn wanted(&self, hash: &BlobHash) -> bool {
        self.wanted.contains(hash)
    }

    /// A blob could not be had; it is not asked for again until something wants it anew.
    pub fn give_up(&mut self, hash: &BlobHash) {
        self.wanted.remove(hash);
    }

    pub fn add_state(&mut self, hash: BlobHash, bytes: Arc<[u8]>) {
        self.wanted.remove(&hash);
        self.states.insert(hash, bytes);
        self.dirty = true;
    }

    pub fn state(&self, hash: &BlobHash) -> Option<Arc<[u8]>> {
        self.states.get(hash).cloned()
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Whether the graph should be built again. Clears the flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Follows the document: instances of devices that are gone are let go, and those whose
    /// state the document changed are to be made anew. Returns the devices to make anew.
    pub fn follow(&mut self, project: &Project) -> Vec<(TrackId, DeviceId)> {
        let mut renew = Vec::new();
        let mut gone = Vec::new();
        for (&(track, device), running) in &self.running {
            let state = project
                .track(track)
                .and_then(|t| t.content.device(device))
                .map(|d| &d.kind);
            match state {
                Some(DeviceKind::Plugin { state, .. }) if *state == running.state => {}
                Some(DeviceKind::Plugin { .. }) => renew.push((track, device)),
                _ => gone.push((track, device)),
            }
        }
        // Instances on their way for devices that are gone are let go when they come.
        let exists = |key: &(TrackId, DeviceId)| {
            project
                .track(key.0)
                .and_then(|t| t.content.device(key.1))
                .is_some()
        };
        self.starting.retain(|key, _| exists(key));
        self.ready.retain(|key, _| exists(key));
        for key in gone.iter().chain(&renew) {
            self.params.remove(key);
            if let Some(running) = self.running.remove(key)
                && let Some(host) = self.host.as_mut()
            {
                host.dispose(running.instance);
            }
        }
        renew
    }

    /// Main-thread chores of every instance; call once per frame. Returns the plugins that
    /// crashed or failed since the last call, each with a word for the user: they are let go,
    /// and their tracks are silent once the graph has forgotten them.
    pub fn idle(&mut self) -> Vec<((TrackId, DeviceId), String)> {
        for running in self.running.values_mut() {
            running.instance.idle();
        }
        let mut done = Vec::new();
        for (key, starting) in &self.starting {
            match starting.result.try_recv() {
                Ok(result) => done.push((*key, Some(result))),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => done.push((*key, None)),
            }
        }
        for (key, result) in done {
            let Some(starting) = self.starting.remove(&key) else {
                continue;
            };
            match result {
                Some(Ok(made)) => {
                    self.ready.insert(key, (starting.state, made));
                }
                Some(Err(text)) => {
                    self.crashed.insert(key, starting.name);
                    self.failures.push((key, text));
                }
                None => {
                    self.failures
                        .push((key, format!("{} did not start.", starting.name)));
                    self.crashed.insert(key, starting.name);
                }
            }
            self.dirty = true;
        }
        let crashed: Vec<(TrackId, DeviceId)> = self
            .running
            .iter()
            .filter(|(_, r)| r.instance.crashed())
            .map(|(key, _)| *key)
            .collect();
        let mut names = std::mem::take(&mut self.failures);
        for key in crashed {
            let Some(running) = self.running.remove(&key) else {
                continue;
            };
            let name = running.instance.info().name.clone();
            self.crashed.insert(key, name.clone());
            names.push((key, format!("{name} crashed or stopped responding.")));
            self.dirty = true;
            if let Some(host) = self.host.as_mut() {
                host.dispose(running.instance);
            }
        }
        if let Some(host) = self.host.as_mut() {
            host.idle();
        }
        names
    }

    /// Saves the state of every instance whose settings may have changed: one that says so,
    /// or whose own window is open (some plugins do not say, as Vital for its presets). Returns
    /// the states that differ from what the instance last stood for; each instance now stands
    /// for its new state, which belongs in the document.
    pub fn save_changed(&mut self) -> Vec<((TrackId, DeviceId), SavedState)> {
        let keys: Vec<(TrackId, DeviceId)> = self
            .running
            .iter()
            .filter(|(_, r)| r.instance.changed() || r.instance.editor_open())
            .map(|(key, _)| *key)
            .collect();
        let mut saved = Vec::new();
        for key in keys {
            // A plugin that will not save now is asked again next time.
            if let Ok(Some(state)) = self.capture(key.0, key.1) {
                saved.push((key, state));
            }
        }
        saved
    }

    /// Whether a device's plugin crashed and waits to be restarted.
    pub fn crashed(&self, track: TrackId, device: DeviceId) -> bool {
        self.crashed.contains_key(&(track, device))
    }

    /// Starts a crashed plugin again, with the settings the document has for it.
    pub fn restart(&mut self, track: TrackId, device: DeviceId) {
        if self.crashed.remove(&(track, device)).is_some() {
            self.dirty = true;
        }
    }

    fn close_all(&mut self) {
        for (_, running) in self.running.drain() {
            if let Some(host) = self.host.as_mut() {
                host.dispose(running.instance);
            }
        }
    }

    /// Whether a device's plugin is being started.
    pub fn loading(&self, track: TrackId, device: DeviceId) -> bool {
        self.starting.contains_key(&(track, device)) || self.ready.contains_key(&(track, device))
    }

    /// Whether a device's plugin runs here.
    pub fn running(&self, track: TrackId, device: DeviceId) -> bool {
        self.running.contains_key(&(track, device))
    }

    /// How `track` is heard here.
    pub fn sound(&self, project: &Project, track: TrackId) -> TrackSound {
        let Some(t) = project.track(track) else {
            return TrackSound::Silent;
        };
        let mut plugins = t.content.devices.iter().filter_map(|d| match &d.kind {
            DeviceKind::Plugin { format, id, .. } => Some((d.id, *format, id.as_str())),
            DeviceKind::Builtin { .. } => None,
        });
        if plugins.clone().next().is_none() {
            return TrackSound::Builtin;
        }
        if t.content.devices.iter().any(|d| self.crashed(track, d.id)) {
            return TrackSound::Crashed;
        }
        let loading = |d: &mdaw_model::Device| {
            self.starting.contains_key(&(track, d.id)) || self.ready.contains_key(&(track, d.id))
        };
        if t.content.devices.iter().any(loading) {
            return TrackSound::Loading;
        }
        if plugins
            .clone()
            .all(|(id, ..)| self.running.contains_key(&(track, id)))
        {
            return TrackSound::Live;
        }
        if plugins.any(|(_, format, id)| self.installed(format, id).is_none()) {
            return TrackSound::NotInstalled;
        }
        TrackSound::Silent
    }

    /// The instance of a device, if it runs here.
    pub fn instance(&mut self, track: TrackId, device: DeviceId) -> Option<&mut dyn Instance> {
        Some(self.running.get_mut(&(track, device))?.instance.as_mut())
    }

    /// The first `limit` parameters of a running instance, and how many it has.
    pub fn params(
        &mut self,
        track: TrackId,
        device: DeviceId,
        limit: usize,
    ) -> Option<(Vec<ParamInfo>, usize)> {
        // Read again only now and then: for a plugin in a helper each read is a message.
        if let Some((at, params, count)) = self.params.get(&(track, device))
            && at.elapsed() < PARAMS_FRESH
            && params.len() >= limit.min(*count)
        {
            return Some((params.iter().take(limit).cloned().collect(), *count));
        }
        let instance = self.instance(track, device)?;
        let (params, count) = (instance.params(limit), instance.param_count());
        self.params
            .insert((track, device), (Instant::now(), params.clone(), count));
        Some((params, count))
    }

    /// Sets a parameter of a running instance, as a user turning its knob.
    pub fn set_param(&mut self, track: TrackId, device: DeviceId, param: u32, value: f64) {
        self.params.remove(&(track, device));
        if let Some(instance) = self.instance(track, device) {
            instance.set_param(param, value);
        }
    }

    /// Whether the main loop has to run often for the plugins: some run on its thread (and with
    /// their window open, very often), or some are starting and nobody wakes it when they are
    /// done.
    pub fn activity(&self) -> (bool, bool) {
        let here = self.running.values().filter(|r| r.instance.needs_main_loop());
        let (mut running, mut editor) = (false, false);
        for r in here {
            running = true;
            editor |= r.instance.editor_open();
        }
        let unwoken = self.waker.is_none()
            && (!self.starting.is_empty() || self.running.values().any(|r| !r.instance.needs_main_loop()));
        (running || unwoken, editor)
    }

    /// Saves the state of a running instance if it changed since it was made or last saved, or
    /// if the document has none yet. Returns the new state, which the instance now stands for.
    pub fn capture(&mut self, track: TrackId, device: DeviceId) -> Result<Option<SavedState>, String> {
        let Some(running) = self.running.get_mut(&(track, device)) else {
            return Ok(None);
        };
        // Saved and compared, whatever the plugin says about itself.
        let bytes: Arc<[u8]> = running.instance.save_state().map_err(|e| e.to_string())?.into();
        let hash = mdaw_proto::blob_hash(&bytes);
        if running.state.is_some() && running.baseline == Some(hash) {
            return Ok(None);
        }
        running.state = Some(hash);
        running.baseline = Some(hash);
        self.states.insert(hash, bytes.clone());
        Ok(Some((hash, bytes)))
    }
}

impl PluginSource for Plugins {
    fn device(
        &mut self,
        track: TrackId,
        device: DeviceId,
        plugin: PluginRef<'_>,
    ) -> Option<Box<dyn ExternalDevice>> {
        if self.crashed.contains_key(&(track, device)) {
            return None;
        }
        let host = self.host.as_mut()?;
        host.find(plugin.format, plugin.id)?;
        let state = match plugin.state {
            Some(hash) => match self.states.get(&hash) {
                Some(bytes) => Some(bytes.clone()),
                None => {
                    self.wanted.insert(hash);
                    return None;
                }
            },
            None => None,
        };
        if let Some(old) = self.running.remove(&(track, device)) {
            host.dispose(old.instance);
        }
        let settings = Settings {
            sample_rate: self.sample_rate,
            max_block: MAX_BLOCK,
            offline: false,
        };
        let name = host
            .find(plugin.format, plugin.id)
            .map_or_else(|| plugin.id.to_string(), |p| p.name.clone());
        let key = (track, device);
        // Made on another thread, and now ready for the graph.
        if let Some((made_for, made)) = self.ready.remove(&key) {
            let mut instance: Box<dyn Instance> = made.instance;
            if made_for == plugin.state
                && let Some(processor) = instance.take_processor()
            {
                self.running.insert(
                    key,
                    Running {
                        instance,
                        state: plugin.state,
                        baseline: made.baseline,
                    },
                );
                return Some(processor);
            }
            if let Some(host) = self.host.as_mut() {
                host.dispose(instance);
            }
        }
        if self.starting.get(&key).is_some_and(|s| s.state == plugin.state) {
            return None;
        }
        let host = self.host.as_mut()?;
        // Plugins in helpers are started on another thread, several at once, and the graph
        // goes on without them until they are there.
        match host.create_later(plugin.format, plugin.id, settings) {
            Some(Ok(creation)) => {
                let (tx, result) = channel();
                let settings_bytes = state.clone();
                let (waker, called) = (self.waker.clone(), name.clone());
                std::thread::spawn(move || {
                    let made = (|| {
                        let mut instance = creation().map_err(|e| format!("{called} did not start: {e}."))?;
                        if let Some(bytes) = &settings_bytes {
                            instance
                                .load_state(bytes)
                                .map_err(|e| format!("{called} did not take its settings: {e}."))?;
                        }
                        let baseline = instance.save_state().ok().map(|b| mdaw_proto::blob_hash(&b));
                        Ok(Made { instance, baseline })
                    })();
                    let _ = tx.send(made);
                    if let Some(waker) = waker {
                        waker();
                    }
                });
                self.starting.insert(
                    key,
                    Starting {
                        state: plugin.state,
                        name: name.clone(),
                        result,
                    },
                );
                return None;
            }
            Some(Err(err)) => {
                self.crashed.insert(key, name.clone());
                self.failures.push((key, format!("{name} did not start: {err}.")));
                return None;
            }
            None => {}
        }
        // A plugin that does not start, or will not take its settings, is not tried again on
        // every edit, and does not play with the wrong settings: like a crash, it waits for
        // Restart.
        let fail = |plugins: &mut Self, text: String| {
            plugins.crashed.insert((track, device), name.clone());
            plugins.failures.push(((track, device), text));
            None
        };
        let mut instance = match host.create(plugin.format, plugin.id, settings) {
            Ok(instance) => instance,
            Err(err) => return fail(self, format!("{name} did not start: {err}.")),
        };
        if let Some(bytes) = state.as_ref()
            && let Err(err) = instance.load_state(bytes)
        {
            if let Some(host) = self.host.as_mut() {
                host.dispose(instance);
            }
            return fail(self, format!("{name} did not take its settings: {err}."));
        }
        let Some(processor) = instance.take_processor() else {
            return fail(self, format!("{name} did not start."));
        };
        let baseline = instance.save_state().ok().map(|b| mdaw_proto::blob_hash(&b));
        self.running.insert(
            (track, device),
            Running {
                instance,
                state: plugin.state,
                baseline,
            },
        );
        Some(processor)
    }
}

/// Shares [`Plugins`] between the session and the engine, both on the UI thread.
#[derive(Clone)]
pub struct Shared(pub Rc<RefCell<Plugins>>);

impl PluginSource for Shared {
    fn device(
        &mut self,
        track: TrackId,
        device: DeviceId,
        plugin: PluginRef<'_>,
    ) -> Option<Box<dyn ExternalDevice>> {
        self.0.borrow_mut().device(track, device, plugin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdaw_model::{ActorId, IdGen};

    #[test]
    fn a_plugin_that_will_not_take_its_settings_is_reported_not_played_wrong() {
        let mut plugins = Plugins::new(48_000);
        plugins.set_host(Some(Host::test_synth_only()));
        let mut ids = IdGen::new(ActorId(3));
        let (track, device) = (ids.track(), ids.device());
        let garbage: Arc<[u8]> = Arc::from(&b"not a state of the test synth"[..]);
        let hash = mdaw_proto::blob_hash(&garbage);
        plugins.add_state(hash, garbage);
        let plugin = PluginRef {
            format: PluginFormat::Clap,
            id: mdaw_plugin::TEST_SYNTH,
            state: Some(hash),
        };
        assert!(
            plugins.device(track, device, plugin).is_none(),
            "not played with other settings"
        );
        assert!(plugins.crashed(track, device));
        let told = plugins.idle();
        assert_eq!(told.len(), 1);
        assert!(told[0].1.contains("did not take its settings"), "{told:?}");
        // Not tried again on every rebuild of the graph, until restarted.
        assert!(plugins.device(track, device, plugin).is_none());
        assert!(plugins.idle().is_empty(), "said once");
        plugins.restart(track, device);
        assert!(plugins.device(track, device, plugin).is_none());
        assert_eq!(
            plugins.idle().len(),
            1,
            "tried again on restart, and failed again"
        );
    }

    /// The test synth's state with this balance.
    fn synth_state(balance: f32) -> Arc<[u8]> {
        let mut bytes = b"MDTS".to_vec();
        for value in [0.5f32, balance, 0.0] {
            bytes.extend(value.to_le_bytes());
        }
        bytes.into()
    }

    #[test]
    fn a_change_the_plugin_does_not_announce_is_still_saved() {
        let mut plugins = Plugins::new(48_000);
        plugins.set_host(Some(Host::test_synth_only()));
        let mut ids = IdGen::new(ActorId(3));
        let (track, device) = (ids.track(), ids.device());
        let first = synth_state(0.0);
        let hash = mdaw_proto::blob_hash(&first);
        plugins.add_state(hash, first);
        let plugin = PluginRef {
            format: PluginFormat::Clap,
            id: mdaw_plugin::TEST_SYNTH,
            state: Some(hash),
        };
        let _processor = plugins.device(track, device, plugin).expect("it runs");
        assert_eq!(plugins.capture(track, device).unwrap(), None, "nothing changed");

        // Other settings, as a preset picked in the plugin's window: the plugin does not say
        // that anything changed.
        let instance = plugins.instance(track, device).unwrap();
        instance.load_state(&synth_state(1.0)).unwrap();
        assert!(!instance.changed());
        let (new, bytes) = plugins
            .capture(track, device)
            .unwrap()
            .expect("the change is found");
        assert_ne!(new, hash);
        assert_eq!(&bytes[..], &synth_state(1.0)[..]);
        // And once captured, it is the new baseline.
        assert_eq!(plugins.capture(track, device).unwrap(), None);
    }
}
