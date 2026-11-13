//! The Rust engine behind [`Audio`]: `mdaw-engine` (cpal device, real-time thread), `mdaw-dsp`
//! (graph, Pocket synth) and `mdaw-plugin` (CLAP, in helper processes). Kept for comparison
//! during the move to Tracktion and for the tests that drive a manual engine; it goes when the
//! Tracktion path has passed its manual test. It hosts CLAP only: VST3 and LV2 devices are
//! "not installed" here.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use mdaw_engine::{Engine, LiveInput};
use mdaw_model::{
    BlobHash, BuiltinKind, DeviceId, DeviceKind, PluginFormat, Project, Tick, TickRange, TrackId,
};

use crate::audio::{Audio, DeviceStatus, LiveSink, ParamEntry, PluginEntry, SavedState, TrackSound};
use crate::link::Waker;
use crate::plugins::{self, Plugins};

pub struct OldAudio {
    engine: Engine,
    plugins: plugins::Shared,
    /// When the plugins' settings were last looked at for changes.
    saved_at: Instant,
    waker: Option<Waker>,
}

struct OldLive(LiveInput);

impl LiveSink for OldLive {
    fn note_on(&self, track: TrackId, key: u64, pitch: u8, vel: u8) {
        self.0.note_on(track, key, pitch, vel);
    }

    fn note_off(&self, track: TrackId, key: u64, _pitch: u8) {
        self.0.note_off(track, key);
    }
}

fn entry(info: &mdaw_plugin::PluginInfo) -> PluginEntry {
    PluginEntry {
        format: info.format,
        id: info.id.clone(),
        name: info.name.clone(),
        vendor: info.vendor.clone(),
        instrument: info.instrument,
        location: info.location.clone(),
    }
}

impl OldAudio {
    pub fn new(mut engine: Engine) -> Self {
        let plugins = plugins::Shared(Rc::new(RefCell::new(Plugins::new(engine.sample_rate()))));
        engine.set_plugins(Box::new(plugins.clone()));
        Self {
            engine,
            plugins,
            saved_at: Instant::now(),
            waker: None,
        }
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }

    /// Hosts plugins from now on: tracks whose plugins are installed here are heard. Without
    /// this, every track with plugins is silent.
    pub fn enable_plugins(&mut self, host: mdaw_plugin::Host) {
        let mut plugins = self.plugins.0.borrow_mut();
        plugins.set_host(Some(host));
        if let Some(waker) = &self.waker {
            plugins.set_waker(waker.clone());
        }
        drop(plugins);
        self.engine.refresh();
    }
}

impl From<Engine> for Box<dyn Audio> {
    fn from(engine: Engine) -> Self {
        Box::new(OldAudio::new(engine))
    }
}

impl From<OldAudio> for Box<dyn Audio> {
    fn from(audio: OldAudio) -> Self {
        Box::new(audio)
    }
}

impl Audio for OldAudio {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn set_waker(&mut self, waker: Waker) {
        // Only once plugins are enabled does the host take it (see `enable_plugins`).
        self.waker = Some(waker);
    }

    fn default_instrument(&self) -> BuiltinKind {
        BuiltinKind::Pocket
    }

    fn set_project(&mut self, project: &Project) {
        self.engine.set_project(project);
    }

    fn frame(&mut self, project: &Project) -> Vec<String> {
        self.engine.tick();
        let renew = self.plugins.0.borrow_mut().follow(project);
        for &(track, device) in &renew {
            self.engine.forget(track, device);
        }
        let notices = self.pump();
        if self.plugins.0.borrow_mut().take_dirty() || !renew.is_empty() {
            self.engine.refresh();
        }
        notices
    }

    fn pump(&mut self) -> Vec<String> {
        let crashed = self.plugins.0.borrow_mut().idle();
        let mut notices = Vec::new();
        for ((track, device), what) in crashed {
            // Its processor is dead: the next graph does without it, and the track is silent.
            self.engine.forget(track, device);
            notices.push(format!(
                "{what} Its track is silent here until it is restarted; Restart is in the Device tab."
            ));
        }
        if self.plugins.0.borrow_mut().take_dirty() {
            self.engine.refresh();
        }
        notices
    }

    fn next_frame_in(&self) -> Option<std::time::Duration> {
        // Plugins live on this thread: their windows, timers and callbacks run only when it
        // does, also while nothing changes in this window.
        match self.plugins.0.borrow().activity() {
            (_, true) => Some(plugins::WINDOW_PACE),
            (true, false) => Some(plugins::PACE),
            _ => None,
        }
    }

    fn play(&mut self) {
        self.engine.play();
    }

    fn stop(&mut self) {
        self.engine.stop();
    }

    fn seek(&mut self, tick: Tick) {
        self.engine.seek(tick);
    }

    fn set_loop(&mut self, range: Option<TickRange>) {
        self.engine.set_loop(range);
    }

    fn is_playing(&self) -> bool {
        self.engine.is_playing()
    }

    fn position(&self) -> f64 {
        self.engine.position()
    }

    fn position_heard(&self) -> (f64, i64) {
        self.engine.position_heard()
    }

    fn live(&self) -> Arc<dyn LiveSink> {
        Arc::new(OldLive(self.engine.live_input()))
    }

    fn set_output_gain(&mut self, gain: f32) {
        self.engine.set_output_gain(gain);
    }

    fn output_gain(&self) -> f32 {
        self.engine.output_gain()
    }

    fn take_peak(&mut self) -> [f32; 2] {
        self.engine.take_peak()
    }

    fn description(&self) -> String {
        self.engine.description().to_string()
    }

    fn plugins_enabled(&self) -> bool {
        self.plugins.0.borrow().has_host()
    }

    fn plugin_catalog(&self) -> Vec<PluginEntry> {
        self.plugins.0.borrow().catalog().iter().map(entry).collect()
    }

    fn installed(&self, format: PluginFormat, id: &str) -> Option<PluginEntry> {
        self.plugins.0.borrow().installed(format, id).map(entry)
    }

    fn device_status(&self, project: &Project, track: TrackId, device: DeviceId) -> DeviceStatus {
        let plugins = self.plugins.0.borrow();
        let Some(d) = project.track(track).and_then(|t| t.content.device(device)) else {
            return DeviceStatus::Loading;
        };
        let (format, id) = match &d.kind {
            DeviceKind::Plugin { format, id, .. } => (format, id),
            // The Tracktion engine's instrument is not played here (`mdaw-dsp` leaves it inert).
            DeviceKind::Builtin {
                kind: BuiltinKind::TwoOsc,
                ..
            } => return DeviceStatus::NotInstalled,
            DeviceKind::Builtin { .. } => return DeviceStatus::Builtin,
        };
        if plugins.crashed(track, device) {
            return DeviceStatus::Failed("crashed".into());
        }
        if plugins.loading(track, device) {
            return DeviceStatus::Loading;
        }
        if plugins.running(track, device) {
            return DeviceStatus::Running;
        }
        if plugins.installed(*format, id).is_none() {
            return DeviceStatus::NotInstalled;
        }
        DeviceStatus::WaitingForState
    }

    fn track_sound(&self, project: &Project, track: TrackId) -> TrackSound {
        let silent_here = project.track(track).is_some_and(|t| {
            t.content.devices.iter().any(|d| {
                matches!(
                    d.kind,
                    DeviceKind::Builtin {
                        kind: BuiltinKind::TwoOsc,
                        ..
                    }
                )
            })
        });
        if silent_here {
            return TrackSound::Silent;
        }
        self.plugins.0.borrow().sound(project, track)
    }

    fn plugin_params(
        &mut self,
        track: TrackId,
        device: DeviceId,
        limit: usize,
    ) -> Option<(Vec<ParamEntry>, usize)> {
        let (params, count) = self.plugins.0.borrow_mut().params(track, device, limit)?;
        let params = params
            .into_iter()
            .map(|p| ParamEntry {
                id: p.id.to_string(),
                name: p.name,
                value: p.value,
                min: p.min,
                max: p.max,
                stepped: p.stepped,
                text: String::new(),
            })
            .collect();
        Some((params, count))
    }

    fn set_plugin_param(&mut self, track: TrackId, device: DeviceId, param: &str, value: f64) {
        if let Ok(param) = param.parse::<u32>() {
            self.plugins.0.borrow_mut().set_param(track, device, param, value);
        }
    }

    fn has_editor(&self, track: TrackId, device: DeviceId) -> bool {
        self.plugins
            .0
            .borrow_mut()
            .instance(track, device)
            .is_some_and(|i| i.has_editor())
    }

    fn editor_open(&self, track: TrackId, device: DeviceId) -> bool {
        self.plugins
            .0
            .borrow_mut()
            .instance(track, device)
            .is_some_and(|i| i.editor_open())
    }

    fn toggle_editor(&mut self, track: TrackId, device: DeviceId, title: &str) -> Result<(), String> {
        let mut plugins = self.plugins.0.borrow_mut();
        let Some(instance) = plugins.instance(track, device) else {
            return Ok(());
        };
        if instance.editor_open() {
            instance.close_editor();
            return Ok(());
        }
        let title = format!("{} - {title}", instance.info().name);
        instance.open_editor(&title).map_err(|e| e.to_string())
    }

    fn restart_plugin(&mut self, track: TrackId, device: DeviceId) {
        self.plugins.0.borrow_mut().restart(track, device);
        self.engine.forget(track, device);
        self.engine.refresh();
    }

    fn wanted_blobs(&mut self, asked: &HashSet<BlobHash>) -> Vec<BlobHash> {
        self.plugins.0.borrow_mut().take_wanted(asked)
    }

    fn blob_arrived(&mut self, hash: BlobHash, bytes: Option<Arc<[u8]>>) {
        let mut plugins = self.plugins.0.borrow_mut();
        match bytes {
            Some(bytes) if plugins.wanted(&hash) => plugins.add_state(hash, bytes),
            Some(_) => {}
            None => plugins.give_up(&hash),
        }
    }

    fn changed_states(&mut self, _project: &Project, now: bool) -> Vec<((TrackId, DeviceId), SavedState)> {
        if !now && self.saved_at.elapsed() < plugins::SAVE_EVERY {
            return Vec::new();
        }
        if !now {
            self.saved_at = Instant::now();
        }
        self.plugins.0.borrow_mut().save_changed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdaw_model::{ActorId, Clip, Device, IdGen, Note, Op, TrackContent, TrackShell, apply};

    /// The Tracktion engine's instrument is inert here: silent, said so, and no error; Pocket
    /// next to it still plays.
    #[test]
    fn the_tracktion_instrument_is_silent_here() {
        let mut audio = OldAudio::new(Engine::manual(48_000, false));
        assert_eq!(audio.default_instrument(), BuiltinKind::Pocket);
        let mut ids = IdGen::new(ActorId(3));
        let mut project = Project::default();
        let mut track_with = |project: &mut Project, kind: BuiltinKind| {
            let (track, device) = (ids.track(), ids.device());
            let mut content = TrackContent::default();
            content
                .devices
                .push_back(Device::new(device, crate::builtin_kind(kind)));
            let mut clip = Clip {
                start: Tick(0),
                length: Tick(4 * mdaw_model::PPQ),
                name: "c".into(),
                notes: Default::default(),
            };
            clip.notes.insert(
                ids.note(),
                Note {
                    start: Tick(0),
                    dur: Tick(mdaw_model::PPQ),
                    pitch: 60,
                    vel: 100,
                },
            );
            content.clips.insert(ids.clip(), Arc::new(clip));
            apply(
                project,
                &Op::TrackAdd {
                    id: track,
                    shell: TrackShell {
                        name: "t".into(),
                        color: 0,
                    },
                    content,
                    after: None,
                },
            );
            (track, device)
        };
        let (track, device) = track_with(&mut project, BuiltinKind::TwoOsc);
        let peak = |audio: &mut OldAudio, project: &Project| {
            audio.set_project(project);
            audio.frame(project);
            audio.seek(Tick(0));
            audio.play();
            let samples = audio.engine_mut().pump(24_000).unwrap_or_default();
            audio.stop();
            samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
        };
        assert_eq!(peak(&mut audio, &project), 0.0, "silent");
        assert_eq!(
            audio.device_status(&project, track, device),
            DeviceStatus::NotInstalled
        );
        assert_eq!(audio.track_sound(&project, track), TrackSound::Silent);

        let (pocket, _) = track_with(&mut project, BuiltinKind::Pocket);
        assert!(peak(&mut audio, &project) > 0.01, "Pocket plays next to it");
        assert_eq!(audio.track_sound(&project, pocket), TrackSound::Builtin);
    }
}
