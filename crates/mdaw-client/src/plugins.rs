//! Plugins on this client: which are installed, the running instances of the document's plugin
//! devices, their state blobs, and the stems of tracks played from audio.
//!
//! Whether a track with plugins is heard live or from its stem is decided here, by each client
//! for itself: with every plugin of the track installed and its state at hand it runs them, and
//! otherwise it plays the track's stem. Everything here speaks to `mdaw_plugin`'s format-neutral
//! interface only.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mdaw_dsp::{ExternalDevice, MAX_BLOCK, PluginRef, PluginSource};
use mdaw_model::{BlobHash, DeviceId, DeviceKind, PluginFormat, Project, Stem, TrackId};
use mdaw_plugin::{Host, Instance, ParamInfo, PluginInfo, Settings};

/// A state saved from an instance: its hash and its bytes.
pub type SavedState = (BlobHash, Arc<[u8]>);

/// What a blob that is not here yet is wanted for.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Want {
    State,
    Stem(Stem),
}

/// How a track is heard on this client.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrackSound {
    /// No plugins: built-in devices only.
    Builtin,
    /// Its plugins run here.
    Live,
    /// A plugin is not installed here, or not ready: the stem plays. `stale` when the tempo
    /// changed since it was rendered.
    Stem { stale: bool },
    /// A plugin is missing and there is no stem (yet): silent.
    Silent,
    /// A plugin crashed. The stem plays if there is one (`stem`), else the track is silent.
    Crashed { stem: bool },
}

/// How often the settings of a plugin that changed are kept aside, for when it crashes.
const KEEP_EVERY: Duration = Duration::from_secs(2);

struct Running {
    instance: Box<dyn Instance>,
    /// The state the instance was made from or last saved to: what the document says, unless
    /// the document has moved on.
    state: Option<BlobHash>,
    /// It was changed since `state`: its settings are not in the document.
    unsaved: bool,
}

pub struct Plugins {
    host: Option<Host>,
    sample_rate: u32,
    running: HashMap<(TrackId, DeviceId), Running>,
    states: HashMap<BlobHash, Arc<[u8]>>,
    /// Decoded stems at the engine's rate, gain applied.
    stems: HashMap<BlobHash, Arc<[f32]>>,
    /// Blobs asked for that are not here, and what for.
    wanted: HashMap<BlobHash, Want>,
    /// Something became available since the graph was built: build it again.
    dirty: bool,
    /// Devices whose plugin crashed or failed, with the plugin's name, until they are
    /// restarted. Their tracks play their stems.
    crashed: HashMap<(TrackId, DeviceId), String>,
    /// Failures to tell the user about at the next `idle`.
    failures: Vec<((TrackId, DeviceId), String)>,
    /// The settings of instances as last kept aside, to come back with after a crash.
    kept: HashMap<(TrackId, DeviceId), Arc<[u8]>>,
    kept_at: Instant,
}

impl Plugins {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            host: None,
            sample_rate,
            running: HashMap::new(),
            states: HashMap::new(),
            stems: HashMap::new(),
            wanted: HashMap::new(),
            dirty: false,
            crashed: HashMap::new(),
            failures: Vec::new(),
            kept: HashMap::new(),
            kept_at: Instant::now(),
        }
    }

    pub fn set_host(&mut self, host: Option<Host>) {
        self.close_all();
        self.host = host;
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
    pub fn take_wanted(&mut self, asked: &HashSet<BlobHash>) -> Vec<(BlobHash, Want)> {
        self.wanted
            .iter()
            .filter(|(hash, _)| !asked.contains(hash))
            .map(|(h, w)| (*h, *w))
            .collect()
    }

    pub fn wanted(&self, hash: &BlobHash) -> Option<Want> {
        self.wanted.get(hash).copied()
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

    /// A stem, decoded, at the engine's rate with its gain applied.
    pub fn add_stem(&mut self, hash: BlobHash, audio: Arc<[f32]>) {
        self.wanted.remove(&hash);
        self.stems.insert(hash, audio);
        self.dirty = true;
    }

    pub fn has_stem(&self, hash: &BlobHash) -> bool {
        self.stems.contains_key(hash)
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
        for key in gone.iter().chain(&renew) {
            // What was kept aside is of another state than the document's now.
            self.kept.remove(key);
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
    /// and their tracks play their stems once the graph has forgotten them.
    pub fn idle(&mut self) -> Vec<((TrackId, DeviceId), String)> {
        for running in self.running.values_mut() {
            running.instance.idle();
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
            names.push((key, format!("{name} crashed.")));
            self.dirty = true;
            if let Some(host) = self.host.as_mut() {
                host.dispose(running.instance);
            }
        }
        if self.kept_at.elapsed() >= KEEP_EVERY {
            self.kept_at = Instant::now();
            self.keep_aside();
        }
        if let Some(host) = self.host.as_mut() {
            host.idle();
        }
        names
    }

    /// Saves the settings of every instance that changed, for after a crash. The plugin then
    /// counts as unchanged; `unsaved` remembers that the document does not have them.
    fn keep_aside(&mut self) {
        for (key, running) in &mut self.running {
            if !running.instance.changed() {
                continue;
            }
            running.unsaved = true;
            if let Ok(bytes) = running.instance.save_state() {
                self.kept.insert(*key, bytes.into());
            }
        }
    }

    /// Whether a device's plugin crashed and waits to be restarted.
    pub fn crashed(&self, track: TrackId, device: DeviceId) -> bool {
        self.crashed.contains_key(&(track, device))
    }

    /// Starts a crashed plugin again, with the settings it had as last kept aside.
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

    /// How `track` is heard here.
    pub fn sound(&self, project: &Project, track: TrackId) -> TrackSound {
        let Some(t) = project.track(track) else {
            return TrackSound::Silent;
        };
        if !t.content.has_plugin() {
            return TrackSound::Builtin;
        }
        if t.content.devices.iter().any(|d| self.crashed(track, d.id)) {
            let stem = t.stem.is_some_and(|s| self.stems.contains_key(&s.blob));
            return TrackSound::Crashed { stem };
        }
        let live = t
            .content
            .devices
            .iter()
            .filter(|d| matches!(d.kind, DeviceKind::Plugin { .. }))
            .all(|d| self.running.contains_key(&(track, d.id)));
        if live {
            return TrackSound::Live;
        }
        match t.stem {
            Some(stem) if self.stems.contains_key(&stem.blob) => TrackSound::Stem {
                stale: stem.tempo_hash != project.tempo_map().hash(),
            },
            _ => TrackSound::Silent,
        }
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
        let instance = self.instance(track, device)?;
        Some((instance.params(limit), instance.param_count()))
    }

    /// Whether any instance runs here, and whether one has its window open.
    pub fn activity(&self) -> (bool, bool) {
        let running = !self.running.is_empty();
        let editor = self.running.values().any(|r| r.instance.editor_open());
        (running, editor)
    }

    /// Saves the state of a running instance if it changed since it was made or last saved, or
    /// if the document has none yet. Returns the new state, which the instance now stands for.
    pub fn capture(&mut self, track: TrackId, device: DeviceId) -> Result<Option<SavedState>, String> {
        let Some(running) = self.running.get_mut(&(track, device)) else {
            return Ok(None);
        };
        if running.state.is_some() && !running.unsaved && !running.instance.changed() {
            return Ok(None);
        }
        let bytes: Arc<[u8]> = running.instance.save_state().map_err(|e| e.to_string())?.into();
        let hash = mdaw_proto::blob_hash(&bytes);
        running.state = Some(hash);
        running.unsaved = false;
        self.kept.remove(&(track, device));
        self.states.insert(hash, bytes.clone());
        Ok(Some((hash, bytes)))
    }

    /// Makes fresh instances for an offline render at `sample_rate`, from the states at hand.
    /// They are kept in `made` for as long as the render runs.
    pub fn offline(&mut self, sample_rate: u32) -> OfflineSource<'_> {
        OfflineSource {
            plugins: self,
            sample_rate,
            made: Vec::new(),
            failures: Vec::new(),
        }
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
        // After a crash, the settings it had, which the document may not have yet.
        let kept = self.kept.get(&(track, device)).cloned();
        let state = match plugin.state {
            Some(hash) => match self.states.get(&hash) {
                Some(bytes) => Some(bytes.clone()),
                None => {
                    self.wanted.insert(hash, Want::State);
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
        // A plugin that does not start, or will not take its settings, is not tried again on
        // every edit, and does not play with the wrong settings: like a crash, it waits for
        // Restart, and the stem plays.
        let fail = |plugins: &mut Self, text: String| {
            plugins.crashed.insert((track, device), name.clone());
            plugins.failures.push(((track, device), text));
            None
        };
        let mut instance = match host.create(plugin.format, plugin.id, settings) {
            Ok(instance) => instance,
            Err(err) => return fail(self, format!("{name} did not start: {err}.")),
        };
        if let Some(bytes) = kept.as_ref().or(state.as_ref())
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
        self.running.insert(
            (track, device),
            Running {
                instance,
                state: plugin.state,
                unsaved: kept.is_some(),
            },
        );
        Some(processor)
    }

    fn stem(&mut self, stem: &Stem) -> Option<Arc<[f32]>> {
        match self.stems.get(&stem.blob) {
            Some(audio) => Some(audio.clone()),
            None => {
                self.wanted.insert(stem.blob, Want::Stem(*stem));
                None
            }
        }
    }
}

/// Plugins for an offline render: new instances, in offline mode, from the states at hand.
pub struct OfflineSource<'a> {
    plugins: &'a mut Plugins,
    sample_rate: u32,
    made: Vec<Box<dyn Instance>>,
    /// What went wrong: a render with a plugin missing is no render of the track.
    failures: Vec<String>,
}

impl OfflineSource<'_> {
    /// The instances made, to keep alive while the render runs, and what failed.
    pub fn finish(self) -> (Vec<Box<dyn Instance>>, Vec<String>) {
        (self.made, self.failures)
    }

    fn make(&mut self, plugin: PluginRef<'_>) -> Result<Box<dyn ExternalDevice>, String> {
        let host = self.plugins.host.as_mut().ok_or("plugins are switched off")?;
        let name = host
            .find(plugin.format, plugin.id)
            .map_or_else(|| plugin.id.to_string(), |p| p.name.clone());
        let state = match plugin.state {
            Some(hash) => Some(
                self.plugins
                    .states
                    .get(&hash)
                    .ok_or_else(|| format!("the settings of {name} are not here"))?
                    .clone(),
            ),
            None => None,
        };
        let settings = Settings {
            sample_rate: self.sample_rate,
            max_block: MAX_BLOCK,
            offline: true,
        };
        let mut instance = host
            .create(plugin.format, plugin.id, settings)
            .map_err(|e| format!("{name} did not start: {e}"))?;
        if let Some(bytes) = &state {
            instance
                .load_state(bytes)
                .map_err(|e| format!("{name} did not take its settings: {e}"))?;
        }
        let processor = instance
            .take_processor()
            .ok_or_else(|| format!("{name} did not start"))?;
        self.made.push(instance);
        Ok(processor)
    }
}

impl PluginSource for OfflineSource<'_> {
    fn device(&mut self, _: TrackId, _: DeviceId, plugin: PluginRef<'_>) -> Option<Box<dyn ExternalDevice>> {
        match self.make(plugin) {
            Ok(processor) => Some(processor),
            Err(failure) => {
                self.failures.push(failure);
                None
            }
        }
    }

    fn stem(&mut self, _: &Stem) -> Option<Arc<[f32]>> {
        None
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

    fn stem(&mut self, stem: &Stem) -> Option<Arc<[f32]>> {
        self.0.borrow_mut().stem(stem)
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
}
