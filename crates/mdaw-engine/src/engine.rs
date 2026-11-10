use crate::alloc_guard;
use crate::rt::{Command, RtCore, Shared};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use mdaw_dsp::{Compiler, Graph};
use mdaw_model::{Project, Tick, TickRange, TrackId};
use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

/// Capacity of the command ring.
const COMMANDS: usize = 1024;
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

fn err(e: impl fmt::Display) -> EngineError {
    EngineError(e.to_string())
}

enum Backend {
    /// A running audio device. The stream owns the real-time core.
    Device(#[allow(dead_code)] cpal::Stream),
    /// No device: the owner renders by calling [`Engine::pump`].
    Manual {
        core: Box<RtCore>,
        clock: Option<Instant>,
    },
}

/// Counters for diagnostics.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub callbacks: u64,
    pub frames: u64,
    pub max_callback_micros: u64,
    pub overruns: u64,
    pub stream_errors: u64,
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
}

struct Parts {
    core: RtCore,
    commands: rtrb::Producer<Command>,
    garbage: rtrb::Consumer<Box<Graph>>,
    shared: Arc<Shared>,
}

fn parts(sample_rate: u32) -> Parts {
    let (commands, command_rx) = rtrb::RingBuffer::new(COMMANDS);
    let (garbage_tx, garbage) = rtrb::RingBuffer::new(MAX_GRAPHS_IN_FLIGHT + 1);
    let shared = Arc::new(Shared::default());
    let core = RtCore::new(
        Box::new(Graph::empty(sample_rate)),
        command_rx,
        garbage_tx,
        shared.clone(),
    );
    Parts {
        core,
        commands,
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
            garbage,
            shared,
            Backend::Device(stream),
        ))
    }

    /// An engine without an audio device. With `real_time`, [`Engine::tick`] renders (and
    /// discards) as much audio as wall-clock time has passed, so the transport moves as if a
    /// device were attached. Without it, only [`Engine::pump`] advances time.
    pub fn manual(sample_rate: u32, real_time: bool) -> Engine {
        let Parts {
            core,
            commands,
            garbage,
            shared,
        } = parts(sample_rate);
        let backend = Backend::Manual {
            core: Box::new(core),
            clock: real_time.then(Instant::now),
        };
        let description = format!("no audio device, {sample_rate} Hz");
        Self::assemble(sample_rate, description, commands, garbage, shared, backend)
    }

    fn assemble(
        sample_rate: u32,
        description: String,
        commands: rtrb::Producer<Command>,
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
        }
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
        self.flush();
        if let Backend::Manual {
            core,
            clock: Some(clock),
        } = &mut self.backend
        {
            let frames = (clock.elapsed().as_secs_f64() * self.sample_rate as f64) as usize;
            *clock = Instant::now();
            let mut sink = [0.0f32; 1024];
            let mut left = frames.min(self.sample_rate as usize);
            while left > 0 {
                let n = left.min(sink.len() / 2);
                core.fill(&mut sink[..n * 2], 2, |s| s);
                left -= n;
            }
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
            let graph = Box::new(self.compiler.build(&project, None));
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

    /// Playhead in ticks.
    pub fn position(&self) -> f64 {
        f64::from_bits(self.shared.tick.load(Relaxed))
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
        }
    }

    /// Renders `frames` of interleaved stereo on the calling thread. Only for engines made with
    /// [`Engine::manual`]; returns `None` when a device is attached.
    pub fn pump(&mut self, frames: usize) -> Option<Vec<f32>> {
        self.flush();
        let Backend::Manual { core, .. } = &mut self.backend else {
            return None;
        };
        let mut out = vec![0.0f32; frames * 2];
        {
            let _realtime = alloc_guard::enter();
            core.fill(&mut out, 2, |s| s);
        }
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
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let _realtime = alloc_guard::enter();
                let started = Instant::now();
                core.fill(data, channels, T::from_sample);
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
