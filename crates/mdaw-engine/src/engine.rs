use crate::rt::{Anchor, Command, LiveNote, RtCore, Shared};
use crate::{alloc_guard, clock};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use mdaw_dsp::{Compiler, Graph, NoPlugins, PluginSource};
use mdaw_model::{DeviceId, Project, Tick, TickRange, TrackId};
use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Output latencies beyond this are taken for a misreport.
const MAX_LATENCY: Duration = Duration::from_secs(1);
/// Capacity of the command ring.
const COMMANDS: usize = 1024;
/// Capacity of the ring for live notes.
const LIVE_NOTES: usize = 1024;
/// Graphs that may be on their way to or from the audio thread at once.
const MAX_GRAPHS_IN_FLIGHT: usize = 16;

#[derive(Debug)]
pub struct EngineError(String);

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EngineError {}

pub(crate) fn err(e: impl fmt::Display) -> EngineError {
    EngineError(e.to_string())
}

enum Backend {
    /// A running audio device. The stream owns the real-time core.
    Device(#[allow(dead_code)] cpal::Stream),
    /// No device: the owner renders by calling [`Engine::pump`].
    Manual { core: Box<RtCore>, clock: ManualClock },
}

/// Time as an engine without a device sees it: the frames rendered so far, counted from when
/// it was made. Counting frames instead of measuring each step loses no fractions.
struct ManualClock {
    /// Follow the real clock, by rendering what is due on every [`Engine::tick`]. Otherwise
    /// time passes only in [`Engine::pump`].
    real_time: bool,
    start_micros: i64,
    frames: u64,
}

impl ManualClock {
    /// When the next frame to be rendered is heard.
    fn micros(&self, sample_rate: u32) -> i64 {
        self.start_micros + (self.frames as u128 * 1_000_000 / sample_rate as u128) as i64
    }
}

/// Counters for diagnostics.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub callbacks: u64,
    pub frames: u64,
    pub max_callback_micros: u64,
    pub overruns: u64,
    pub stream_errors: u64,
    /// Jumps made to get back in step with a followed transport.
    pub resyncs: u64,
}

/// Control-side handle of the audio engine. Lives on the UI thread; every method returns
/// immediately.
pub struct Engine {
    sample_rate: u32,
    description: String,
    compiler: Compiler,
    commands: rtrb::Producer<Command>,
    garbage: rtrb::Consumer<Box<Graph>>,
    shared: Arc<Shared>,
    backlog: VecDeque<Command>,
    graphs_in_flight: usize,
    /// Snapshot the audio thread has (or is about to get).
    current: Option<Project>,
    /// Newer snapshot waiting for room in the ring.
    waiting: Option<Project>,
    playing: bool,
    backend: Backend,
    /// The `id` of the last preview sent.
    preview_seq: u64,
    /// Previews the audio thread may still hold, by `id`.
    previews: Vec<(u64, Arc<[f32]>)>,
    live: LiveInput,
    /// Where plugin devices and stems come from. `None`: nowhere.
    plugins: Option<Box<dyn PluginSource>>,
}

/// Plays live notes from any thread, straight into the audio thread: a note played on a MIDI
/// device or arriving from the network does not wait for the UI. Cheap to clone.
#[derive(Clone)]
pub struct LiveInput {
    ring: Arc<Mutex<rtrb::Producer<LiveNote>>>,
}

impl LiveInput {
    /// Starts a note on `track`. `key` identifies it for [`LiveInput::note_off`]; keys are
    /// shared with [`Engine::note_on`]. False if the note was dropped because the audio thread
    /// is far behind.
    pub fn note_on(&self, track: TrackId, key: u64, pitch: u8, vel: u8) -> bool {
        self.push(LiveNote::On {
            track,
            key,
            pitch,
            vel,
        })
    }

    pub fn note_off(&self, track: TrackId, key: u64) -> bool {
        self.push(LiveNote::Off { track, key })
    }

    fn push(&self, note: LiveNote) -> bool {
        let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        ring.push(note).is_ok()
    }
}

struct Parts {
    core: RtCore,
    commands: rtrb::Producer<Command>,
    live: LiveInput,
    garbage: rtrb::Consumer<Box<Graph>>,
    shared: Arc<Shared>,
}

fn parts(sample_rate: u32) -> Parts {
    // Start the clock before the audio thread reads it.
    clock::now_micros();
    let (commands, command_rx) = rtrb::RingBuffer::new(COMMANDS);
    let (live, live_rx) = rtrb::RingBuffer::new(LIVE_NOTES);
    let (garbage_tx, garbage) = rtrb::RingBuffer::new(MAX_GRAPHS_IN_FLIGHT + 1);
    let shared = Arc::new(Shared::default());
    shared.output_gain.store(1.0f32.to_bits(), Relaxed);
    let core = RtCore::new(
        Box::new(Graph::empty(sample_rate)),
        command_rx,
        live_rx,
        garbage_tx,
        shared.clone(),
    );
    Parts {
        core,
        commands,
        live: LiveInput {
            ring: Arc::new(Mutex::new(live)),
        },
        garbage,
        shared,
    }
}

impl Engine {
    /// Opens the default output device and starts the audio thread.
    pub fn open() -> Result<Engine, EngineError> {
        let host = cpal::available_hosts()
            .into_iter()
            .find(|h| h.name().eq_ignore_ascii_case("pipewire"))
            .and_then(|h| cpal::host_from_id(h).ok())
            .unwrap_or_else(cpal::default_host);
        let device = host
            .default_output_device()
            .ok_or_else(|| err("no audio output device"))?;
        let supported = device.default_output_config().map_err(err)?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let sample_rate = config.sample_rate;
        let channels = config.channels as usize;
        let description = format!(
            "{} via {}, {} Hz, {} ch",
            device
                .description()
                .map(|d| d.name().to_string())
                .unwrap_or_else(|_| "output".into()),
            host.id().name(),
            sample_rate,
            channels
        );

        let Parts {
            core,
            commands,
            live,
            garbage,
            shared,
        } = parts(sample_rate);
        let stream = match format {
            cpal::SampleFormat::F32 => build::<f32>(&device, config, core, shared.clone()),
            cpal::SampleFormat::I16 => build::<i16>(&device, config, core, shared.clone()),
            cpal::SampleFormat::U16 => build::<u16>(&device, config, core, shared.clone()),
            cpal::SampleFormat::I32 => build::<i32>(&device, config, core, shared.clone()),
            other => Err(err(format!("unsupported sample format {other}"))),
        }?;
        stream.play().map_err(err)?;
        Ok(Self::assemble(
            sample_rate,
            description,
            commands,
            live,
            garbage,
            shared,
            Backend::Device(stream),
        ))
    }

    /// An engine without an audio device. With `real_time`, [`Engine::tick`] renders (and
    /// discards) as much audio as wall-clock time has passed, so the transport moves as if a
    /// device were attached. Without it, only [`Engine::pump`] advances time, and
    /// [`Engine::now_micros`] with it.
    pub fn manual(sample_rate: u32, real_time: bool) -> Engine {
        let Parts {
            core,
            commands,
            live,
            garbage,
            shared,
        } = parts(sample_rate);
        let backend = Backend::Manual {
            core: Box::new(core),
            clock: ManualClock {
                real_time,
                start_micros: clock::now_micros(),
                frames: 0,
            },
        };
        let description = format!("no audio device, {sample_rate} Hz");
        Self::assemble(sample_rate, description, commands, live, garbage, shared, backend)
    }

    fn assemble(
        sample_rate: u32,
        description: String,
        commands: rtrb::Producer<Command>,
        live: LiveInput,
        garbage: rtrb::Consumer<Box<Graph>>,
        shared: Arc<Shared>,
        backend: Backend,
    ) -> Engine {
        Engine {
            sample_rate,
            description,
            compiler: Compiler::new(sample_rate),
            commands,
            garbage,
            shared,
            backlog: VecDeque::new(),
            graphs_in_flight: 0,
            current: None,
            waiting: None,
            playing: false,
            backend,
            preview_seq: 0,
            previews: Vec::new(),
            live,
            plugins: None,
        }
    }

    /// Where plugin devices and stems come from from now on. Builds the graph anew.
    pub fn set_plugins(&mut self, plugins: Box<dyn PluginSource>) {
        self.plugins = Some(plugins);
        self.refresh();
    }

    /// Makes a device anew with the next graph instead of carrying it over, as when a plugin got
    /// another state. Call [`Engine::refresh`] after.
    pub fn forget(&mut self, track: TrackId, device: DeviceId) {
        self.compiler.forget(track, device);
    }

    /// Builds the graph anew from the last document, as when a plugin or a stem has become
    /// available.
    pub fn refresh(&mut self) {
        if let Some(project) = self.current.take().or_else(|| self.waiting.take()) {
            self.waiting = Some(project);
            self.flush();
        }
    }

    /// A handle that plays live notes from any thread.
    pub fn live_input(&self) -> LiveInput {
        self.live.clone()
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Human-readable description of the output.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Hands the engine a new snapshot of the document. Cheap when nothing audible changed.
    pub fn set_project(&mut self, project: &Project) {
        if self.current.as_ref() == Some(project) {
            self.waiting = None;
            return;
        }
        self.waiting = Some(project.clone());
        self.flush();
    }

    /// Housekeeping; call once per UI frame. Frees retired graphs and sends whatever had to wait.
    pub fn tick(&mut self) {
        while let Ok(graph) = self.garbage.pop() {
            drop(graph);
            self.graphs_in_flight = self.graphs_in_flight.saturating_sub(1);
        }
        // The audio thread takes previews in order: older ones than its current are let go.
        let taken = self.shared.preview_id.load(Relaxed);
        self.previews.retain(|(id, _)| *id >= taken);
        self.flush();
        if let Backend::Manual { core, clock } = &mut self.backend
            && clock.real_time
        {
            let rate = self.sample_rate as u64;
            let passed = (clock::now_micros() - clock.start_micros).max(0) as u64;
            let due = (passed as u128 * rate as u128 / 1_000_000) as u64;
            // After a long pause, skip ahead instead of rendering it all.
            clock.frames = clock.frames.max(due.saturating_sub(rate));
            let mut sink = [0.0f32; 1024];
            while clock.frames < due {
                let n = ((due - clock.frames) as usize).min(sink.len() / 2);
                core.fill(&mut sink[..n * 2], 2, clock.micros(self.sample_rate), |s| s);
                clock.frames += n as u64;
            }
        }
    }

    /// The time [`Engine::follow`] is given anchors on: [`clock::now_micros`], except for a
    /// manual engine that is not real-time, whose time is the audio pumped so far.
    pub fn now_micros(&self) -> i64 {
        match &self.backend {
            Backend::Manual { clock, .. } if !clock.real_time => clock.micros(self.sample_rate),
            _ => clock::now_micros(),
        }
    }

    fn flush(&mut self) {
        while let Some(command) = self.backlog.pop_front() {
            if let Err(rtrb::PushError::Full(command)) = self.commands.push(command) {
                self.backlog.push_front(command);
                return;
            }
        }
        if self.waiting.is_some() && self.graphs_in_flight < MAX_GRAPHS_IN_FLIGHT {
            let project = self.waiting.take().expect("checked above");
            // The compiler assumes each graph replaces the one built before it, so a graph is
            // only built when it can be sent.
            let graph = match &mut self.plugins {
                Some(plugins) => self.compiler.build_with(&project, None, plugins.as_mut()),
                None => self.compiler.build_with(&project, None, &mut NoPlugins),
            };
            let graph = Box::new(graph);
            match self.commands.push(Command::SetGraph(graph)) {
                Ok(()) => {
                    self.graphs_in_flight += 1;
                    self.current = Some(project);
                }
                Err(rtrb::PushError::Full(command)) => {
                    self.graphs_in_flight += 1;
                    self.current = Some(project);
                    self.backlog.push_back(command);
                }
            }
        }
    }

    fn send(&mut self, command: Command) {
        self.backlog.push_back(command);
        self.flush();
    }

    pub fn play(&mut self) {
        self.playing = true;
        self.send(Command::Play);
    }

    pub fn stop(&mut self) {
        self.playing = false;
        self.send(Command::Stop);
    }

    /// Plays in step with a transport kept elsewhere: the playhead is at `tick` at `at_micros`
    /// on [`Engine::now_micros`]. If that is still to come, the playhead waits at `tick` and
    /// starts on time; if it has passed, playback is wherever that puts it by now. The engine
    /// then keeps to it, jumping when it finds itself more than 20 ms off.
    ///
    /// `fresh` says the anchor describes a new playback, as after a seek, and not a better
    /// estimate of the same one; the playhead then moves there without delay.
    pub fn follow(&mut self, tick: f64, at_micros: i64, fresh: bool) {
        self.playing = true;
        let anchor = Anchor {
            tick,
            micros: at_micros,
        };
        self.send(Command::Follow { anchor, fresh });
    }

    /// Stops following. Playback goes on freely from where it is.
    pub fn unfollow(&mut self) {
        self.send(Command::Unfollow);
    }

    /// While following, once started: how far the playhead is ahead of where the anchor puts
    /// it, in microseconds.
    pub fn drift_micros(&self) -> Option<i64> {
        self.shared
            .following
            .load(Relaxed)
            .then(|| self.shared.drift_micros.load(Relaxed))
    }

    /// What the transport was last told, without waiting for the audio thread to catch up.
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn seek(&mut self, tick: Tick) {
        // Publish at once, so the UI does not show the old position for a frame.
        self.shared.tick.store((tick.0.max(0) as f64).to_bits(), Relaxed);
        self.send(Command::Seek(tick.0 as f64));
    }

    pub fn set_loop(&mut self, range: Option<TickRange>) {
        let range = range
            .filter(|r| !r.is_empty())
            .map(|r| (r.start.0 as f64, r.end.0 as f64));
        self.send(Command::SetLoop(range));
    }

    /// Starts a live note (keyboard, MIDI, audition). `key` identifies it for `note_off`.
    pub fn note_on(&mut self, track: TrackId, key: u64, pitch: u8, vel: u8) {
        self.send(Command::NoteOn {
            track,
            key,
            pitch,
            vel,
        });
    }

    pub fn note_off(&mut self, track: TrackId, key: u64) {
        self.send(Command::NoteOff { track, key });
    }

    /// Ends every sounding note.
    pub fn panic(&mut self) {
        self.send(Command::Panic);
    }

    /// Plays interleaved stereo at the engine's rate on its own, beside whatever else plays,
    /// replacing an earlier preview. Returns an `id` for [`Engine::preview_position`].
    pub fn preview(&mut self, audio: Arc<[f32]>) -> u64 {
        self.preview_seq += 1;
        let id = self.preview_seq;
        self.previews.push((id, audio.clone()));
        self.send(Command::Preview {
            id,
            audio: Some(audio),
        });
        id
    }

    pub fn stop_preview(&mut self) {
        self.preview_seq += 1;
        let id = self.preview_seq;
        self.send(Command::Preview { id, audio: None });
    }

    /// How far preview `id` has played, in seconds. `None` once it is over, replaced or
    /// stopped. A preview just started counts as at its beginning.
    pub fn preview_position(&self, id: u64) -> Option<f64> {
        if id != self.preview_seq {
            return None;
        }
        if self.shared.preview_id.load(Relaxed) != id {
            return Some(0.0);
        }
        match self.shared.preview_pos.load(Relaxed) {
            u64::MAX => None,
            frames => Some(frames as f64 / self.sample_rate as f64),
        }
    }

    /// Playhead in ticks.
    pub fn position(&self) -> f64 {
        f64::from_bits(self.shared.tick.load(Relaxed))
    }

    /// Sets the listener's own volume: applied to everything this engine plays, and to nothing
    /// that is rendered or shared.
    pub fn set_output_gain(&mut self, gain: f32) {
        let gain = if gain.is_finite() {
            gain.clamp(0.0, 4.0)
        } else {
            1.0
        };
        self.shared.output_gain.store(gain.to_bits(), Relaxed);
    }

    pub fn output_gain(&self) -> f32 {
        f32::from_bits(self.shared.output_gain.load(Relaxed))
    }

    /// Output peak per channel since the last call.
    pub fn take_peak(&self) -> [f32; 2] {
        [0, 1].map(|ch| f32::from_bits(self.shared.peak[ch].swap(0, Relaxed)))
    }

    pub fn stats(&self) -> Stats {
        Stats {
            callbacks: self.shared.callbacks.load(Relaxed),
            frames: self.shared.frames.load(Relaxed),
            max_callback_micros: self.shared.max_micros.load(Relaxed),
            overruns: self.shared.overruns.load(Relaxed),
            stream_errors: self.shared.stream_errors.load(Relaxed),
            resyncs: self.shared.resyncs.load(Relaxed),
        }
    }

    /// Renders `frames` of interleaved stereo on the calling thread. Only for engines made with
    /// [`Engine::manual`]; returns `None` when a device is attached.
    pub fn pump(&mut self, frames: usize) -> Option<Vec<f32>> {
        self.flush();
        let Backend::Manual { core, clock } = &mut self.backend else {
            return None;
        };
        let mut out = vec![0.0f32; frames * 2];
        {
            let _realtime = alloc_guard::enter();
            core.fill(&mut out, 2, clock.micros(self.sample_rate), |s| s);
        }
        clock.frames += frames as u64;
        Some(out)
    }
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut core: RtCore,
    shared: Arc<Shared>,
) -> Result<cpal::Stream, EngineError>
where
    T: cpal::SizedSample + cpal::FromSample<f32> + Send + 'static,
{
    let channels = config.channels as usize;
    let sample_rate = core.sample_rate() as u64;
    let errors = shared.clone();
    device
        .build_output_stream(
            config,
            move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
                let _realtime = alloc_guard::enter();
                let started = Instant::now();
                // The device says how long after this call the buffer is heard.
                let stamp = info.timestamp();
                let latency = stamp.playback.duration_since(stamp.callback).min(MAX_LATENCY);
                let play_micros = clock::now_micros() + latency.as_micros() as i64;
                core.fill(data, channels, play_micros, T::from_sample);
                let micros = started.elapsed().as_micros() as u64;
                shared.max_micros.fetch_max(micros, Relaxed);
                let budget = (data.len() / channels.max(1)) as u64 * 1_000_000 / sample_rate;
                if micros > budget {
                    shared.overruns.fetch_add(1, Relaxed);
                }
            },
            move |error| {
                errors.stream_errors.fetch_add(1, Relaxed);
                eprintln!("audio stream: {error}");
            },
            None,
        )
        .map_err(err)
}
