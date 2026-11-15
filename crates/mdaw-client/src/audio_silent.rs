//! An engine that plays nothing, behind [`Audio`]: for tests and tools that need a session
//! without sound, and for the window when Tracktion Engine does not start. It is also the only
//! way to have several sessions in one process (Tracktion allows one engine per process).
//!
//! It keeps a transport as an engine would: play, stop, seek and a loop, the playhead moving at
//! the project's tempo while it plays, with the wall clock ([`SilentAudio::new`]) or only when
//! told to ([`SilentAudio::manual`] and [`SilentAudio::advance`], for tests that must not
//! depend on timing). It offers no plugins; its built-in instrument is the Tracktion one, which
//! it shows but does not play.

use std::any::Any;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mdaw_model::{BuiltinKind, DeviceId, PluginFormat, Project, TempoMap, Tick, TickRange, TrackId};

use crate::audio::{
    Audio, DeviceStatus, EditorState, LiveSink, ParamEntry, PluginEntry, SavedState, TrackSound,
};
use crate::clock;

/// The notes played live and not let go yet: `(track, key, pitch)`.
type Held = Arc<Mutex<Vec<(TrackId, u64, u8)>>>;

pub struct SilentAudio {
    tempo: TempoMap,
    playing: bool,
    /// The playhead in seconds on the tempo map at `since`...
    at: f64,
    /// ...and when that was, if the playhead moves with the clock.
    since: Option<Instant>,
    /// Whether only [`SilentAudio::advance`] moves the playhead.
    manual: bool,
    looped: Option<TickRange>,
    output_gain: f32,
    held: Held,
}

struct SilentLive(Held);

impl LiveSink for SilentLive {
    fn note_on(&self, track: TrackId, key: u64, pitch: u8, _vel: u8) {
        let mut held = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !held.contains(&(track, key, pitch)) {
            held.push((track, key, pitch));
        }
    }

    fn note_off(&self, track: TrackId, key: u64, pitch: u8) {
        let mut held = self.0.lock().unwrap_or_else(|e| e.into_inner());
        held.retain(|n| *n != (track, key, pitch));
    }
}

impl Default for SilentAudio {
    fn default() -> Self {
        Self::new()
    }
}

impl SilentAudio {
    /// A transport that runs with the wall clock, as one with an audio device.
    pub fn new() -> Self {
        Self {
            tempo: TempoMap::default(),
            playing: false,
            at: 0.0,
            since: None,
            manual: false,
            looped: None,
            output_gain: 1.0,
            held: Held::default(),
        }
    }

    /// A transport that moves only with [`SilentAudio::advance`].
    pub fn manual() -> Self {
        Self {
            manual: true,
            ..Self::new()
        }
    }

    /// Moves a playing transport on by `time`, as if that much had been played.
    pub fn advance(&mut self, time: Duration) {
        if self.playing {
            let now = self.seconds_now() + time.as_secs_f64();
            self.at = self.wrapped(now);
        }
    }

    /// The live notes sounding now (computer keyboard, MIDI, auditioning): `(track, pitch)`.
    pub fn held(&self) -> Vec<(TrackId, u8)> {
        let held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        held.iter().map(|&(track, _, pitch)| (track, pitch)).collect()
    }

    /// The playhead in seconds, before the loop is taken into account.
    fn seconds_now(&self) -> f64 {
        match self.since {
            Some(since) if self.playing => self.at + since.elapsed().as_secs_f64(),
            _ => self.at,
        }
    }

    /// Where a playhead that got to `seconds` from `self.at` is, going round the loop as the
    /// engine does: only a playhead that was before the loop's end goes round it.
    fn wrapped(&self, seconds: f64) -> f64 {
        let Some(range) = self.looped else {
            return seconds;
        };
        let start = self.tempo.tick_to_seconds(range.start.0 as f64);
        let end = self.tempo.tick_to_seconds(range.end.0 as f64);
        if end <= start || self.at >= end || seconds < end {
            return seconds;
        }
        start + (seconds - end) % (end - start)
    }

    /// Takes the playhead as it is now as the new starting point.
    fn anchor(&mut self) {
        self.at = self.wrapped(self.seconds_now());
        self.since = (!self.manual).then(Instant::now);
    }
}

impl From<SilentAudio> for Box<dyn Audio> {
    fn from(audio: SilentAudio) -> Self {
        Box::new(audio)
    }
}

impl Audio for SilentAudio {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn default_instrument(&self) -> BuiltinKind {
        BuiltinKind::TwoOsc
    }

    fn set_project(&mut self, project: &Project) {
        let tempo = TempoMap::compile(&project.tempo);
        if tempo != self.tempo {
            // The tempo changed: stay at the same musical position.
            let tick = self.position();
            self.anchor();
            self.tempo = tempo;
            self.at = self.tempo.tick_to_seconds(tick);
        }
    }

    fn frame(&mut self, project: &Project) -> Vec<String> {
        self.set_project(project);
        Vec::new()
    }

    fn play(&mut self) {
        if !self.playing {
            self.anchor();
            self.playing = true;
        }
    }

    fn stop(&mut self) {
        if self.playing {
            self.anchor();
            self.playing = false;
        }
    }

    fn seek(&mut self, tick: Tick) {
        self.anchor();
        self.at = self.tempo.tick_to_seconds(tick.0.max(0) as f64);
    }

    fn set_loop(&mut self, range: Option<TickRange>) {
        self.anchor();
        self.looped = range.filter(|r| !r.is_empty());
    }

    fn is_playing(&self) -> bool {
        self.playing
    }

    fn position(&self) -> f64 {
        self.tempo
            .seconds_to_tick(self.wrapped(self.seconds_now()).max(0.0))
    }

    fn position_heard(&self) -> (f64, i64) {
        (self.position(), clock::now_micros())
    }

    fn live(&self) -> Arc<dyn LiveSink> {
        Arc::new(SilentLive(self.held.clone()))
    }

    fn set_output_gain(&mut self, gain: f32) {
        self.output_gain = gain;
    }

    fn output_gain(&self) -> f32 {
        self.output_gain
    }

    fn take_peak(&mut self) -> [f32; 2] {
        [0.0; 2]
    }

    fn description(&self) -> String {
        "No sound: no audio engine".into()
    }

    fn plugins_enabled(&self) -> bool {
        false
    }

    fn plugin_catalog(&self) -> Vec<PluginEntry> {
        Vec::new()
    }

    fn installed(&self, _format: PluginFormat, _id: &str) -> Option<PluginEntry> {
        None
    }

    fn device_status(&self, project: &Project, track: TrackId, device: DeviceId) -> DeviceStatus {
        match project.track(track).and_then(|t| t.content.device(device)) {
            None => DeviceStatus::Loading,
            Some(d) if matches!(d.kind, mdaw_model::DeviceKind::Builtin { .. }) => DeviceStatus::Builtin,
            Some(_) => DeviceStatus::NotInstalled,
        }
    }

    fn track_sound(&self, project: &Project, track: TrackId) -> TrackSound {
        let Some(t) = project.track(track) else {
            return TrackSound::Silent;
        };
        crate::audio::track_sound_from(
            t.content
                .devices
                .iter()
                .map(|d| self.device_status(project, track, d.id)),
        )
    }

    fn plugin_params(
        &mut self,
        _track: TrackId,
        _device: DeviceId,
        _limit: usize,
    ) -> Option<(Vec<ParamEntry>, usize)> {
        None
    }

    fn set_plugin_param(&mut self, _track: TrackId, _device: DeviceId, _param: &str, _value: f64) {}

    fn has_editor(&self, _track: TrackId, _device: DeviceId) -> bool {
        false
    }

    fn editor_open(&self, _track: TrackId, _device: DeviceId) -> bool {
        false
    }

    fn editor_state(&self, _track: TrackId, _device: DeviceId) -> EditorState {
        EditorState::Closed
    }

    fn toggle_editor(&mut self, _track: TrackId, _device: DeviceId, _title: &str) -> Result<(), String> {
        Ok(())
    }

    fn restart_plugin(&mut self, _track: TrackId, _device: DeviceId) {}

    /// It plays no plugins, so it needs no settings.
    fn set_fetcher(&mut self, _fetcher: crate::link::Fetcher) {}

    fn changed_states(&mut self, _project: &Project, _now: bool) -> Vec<((TrackId, DeviceId), SavedState)> {
        Vec::new()
    }

    fn render(&mut self, _path: &Path, _from: Tick, _to: Tick) -> Result<(), String> {
        Err("this engine plays nothing".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdaw_model::{ActorId, IdGen, Op, PPQ, TempoPoint, apply};

    fn at_bpm(bpm: u32) -> Project {
        let mut project = Project::default();
        let point = TempoPoint {
            tick: Tick::ZERO,
            milli_bpm: bpm * 1000,
        };
        apply(
            &mut project,
            &Op::TempoPut {
                id: IdGen::new(ActorId(2)).tempo(),
                point,
            },
        );
        project
    }

    #[test]
    fn the_playhead_moves_at_the_tempo_and_goes_round_the_loop() {
        let mut audio = SilentAudio::manual();
        audio.set_project(&at_bpm(120));
        audio.advance(Duration::from_secs(1));
        assert_eq!(audio.position(), 0.0, "stopped, it stands");
        audio.play();
        audio.advance(Duration::from_millis(500));
        assert_eq!(audio.position(), PPQ as f64, "half a second is a beat at 120 bpm");
        audio.stop();
        audio.advance(Duration::from_secs(1));
        assert_eq!(audio.position(), PPQ as f64);

        // At 60 bpm the same musical position, and half the speed.
        audio.set_project(&at_bpm(60));
        assert_eq!(audio.position(), PPQ as f64);
        audio.play();
        audio.advance(Duration::from_secs(1));
        assert_eq!(audio.position(), 2.0 * PPQ as f64);

        // A loop of one beat from beat 2: round and round.
        audio.set_loop(Some(TickRange::new(Tick::beats(2), Tick::beats(3))));
        audio.advance(Duration::from_millis(1500));
        assert_eq!(audio.position(), 2.5 * PPQ as f64);
        audio.advance(Duration::from_secs(1));
        assert_eq!(audio.position(), 2.5 * PPQ as f64);
        // From past the loop's end it plays on.
        audio.seek(Tick::beats(4));
        audio.advance(Duration::from_secs(1));
        assert_eq!(audio.position(), 5.0 * PPQ as f64);
        assert!(
            audio
                .render(Path::new("/nowhere.wav"), Tick::ZERO, Tick::bars(1))
                .is_err()
        );
    }

    #[test]
    fn with_the_clock_the_playhead_moves_by_itself() {
        let mut audio = SilentAudio::new();
        audio.play();
        std::thread::sleep(Duration::from_millis(30));
        assert!(audio.position() > 0.0);
        let (tick, at) = audio.position_heard();
        assert!(tick > 0.0 && at <= clock::now_micros());
    }

    #[test]
    fn live_notes_are_held_until_let_go() {
        let audio = SilentAudio::manual();
        let track = IdGen::new(ActorId(2)).track();
        audio.live().note_on(track, 1, 60, 100);
        audio.live().note_on(track, 1, 60, 100);
        assert_eq!(audio.held(), [(track, 60)]);
        audio.live().note_off(track, 1, 60);
        assert!(audio.held().is_empty());
    }
}
