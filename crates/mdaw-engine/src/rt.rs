//! The part of the engine that runs inside the audio callback. Nothing here allocates, frees,
//! locks or blocks.

use mdaw_dsp::{Graph, MAX_BLOCK};
use mdaw_model::TrackId;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};

pub(crate) enum Command {
    /// Replace the graph. The old one travels back for disposal.
    SetGraph(Box<Graph>),
    Play,
    Stop,
    /// Jump to a tick.
    Seek(f64),
    /// Loop region in ticks.
    SetLoop(Option<(f64, f64)>),
    NoteOn {
        track: TrackId,
        key: u64,
        pitch: u8,
        vel: u8,
    },
    NoteOff {
        track: TrackId,
        key: u64,
    },
    Panic,
}

/// What the audio thread publishes for everyone else.
#[derive(Default)]
pub(crate) struct Shared {
    /// Playhead in ticks, as `f64` bits.
    pub tick: AtomicU64,
    pub playing: AtomicBool,
    /// Output peak per channel since the UI last read it, as `f32` bits.
    pub peak: [AtomicU32; 2],
    pub callbacks: AtomicU64,
    pub frames: AtomicU64,
    /// Longest callback so far, in microseconds.
    pub max_micros: AtomicU64,
    /// Callbacks that took longer than the audio they produced.
    pub overruns: AtomicU64,
    pub stream_errors: AtomicU64,
}

pub(crate) struct RtCore {
    graph: Box<Graph>,
    commands: rtrb::Consumer<Command>,
    garbage: rtrb::Producer<Box<Graph>>,
    shared: Arc<Shared>,
    playing: bool,
    /// Timeline position in samples.
    pos: i64,
    loop_ticks: Option<(f64, f64)>,
    loop_samples: Option<(i64, i64)>,
    scratch: [f32; MAX_BLOCK * 2],
}

impl RtCore {
    pub fn new(
        graph: Box<Graph>,
        commands: rtrb::Consumer<Command>,
        garbage: rtrb::Producer<Box<Graph>>,
        shared: Arc<Shared>,
    ) -> Self {
        Self {
            graph,
            commands,
            garbage,
            shared,
            playing: false,
            pos: 0,
            loop_ticks: None,
            loop_samples: None,
            scratch: [0.0; MAX_BLOCK * 2],
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.graph.sample_rate()
    }

    fn update_loop(&mut self) {
        self.loop_samples = self.loop_ticks.and_then(|(start, end)| {
            let (start, end) = (self.graph.sample_at(start), self.graph.sample_at(end));
            (end > start).then_some((start, end))
        });
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::SetGraph(mut graph) => {
                if graph.adopt_from(&mut self.graph) {
                    // The tempo map changed: stay at the same musical position.
                    let tick = self.graph.tick_at(self.pos);
                    self.pos = graph.sample_at(tick);
                }
                let old = std::mem::replace(&mut self.graph, graph);
                self.update_loop();
                if let Err(rtrb::PushError::Full(old)) = self.garbage.push(old) {
                    // Freeing here would allocate-lock the audio thread. The control side
                    // limits outstanding graphs, so this does not happen; leak if it does.
                    std::mem::forget(old);
                }
            }
            Command::Play => {
                if !self.playing {
                    self.playing = true;
                    self.graph.chase(self.pos);
                }
            }
            Command::Stop => {
                if self.playing {
                    self.playing = false;
                    self.graph.release_timeline_notes();
                }
            }
            Command::Seek(tick) => {
                self.pos = self.graph.sample_at(tick.max(0.0));
                if self.playing {
                    self.graph.release_timeline_notes();
                    self.graph.chase(self.pos);
                }
            }
            Command::SetLoop(range) => {
                self.loop_ticks = range;
                self.update_loop();
            }
            Command::NoteOn {
                track,
                key,
                pitch,
                vel,
            } => self.graph.note_on(track, key, pitch, vel),
            Command::NoteOff { track, key } => self.graph.note_off(track, key),
            Command::Panic => self.graph.release_all_notes(),
        }
    }

    /// Renders up to `max` frames into the scratch buffer and returns how many.
    fn block(&mut self, max: usize) -> usize {
        let mut frames = max.min(MAX_BLOCK);
        let pos = self.playing.then_some(self.pos);
        if let (Some(pos), Some((_, end))) = (pos, self.loop_samples)
            && pos < end
        {
            frames = frames.min((end - pos) as usize);
        }
        self.graph.process(pos, &mut self.scratch[..frames * 2]);
        if self.playing {
            self.pos += frames as i64;
            if let Some((start, end)) = self.loop_samples
                && self.pos == end
            {
                self.graph.release_timeline_notes();
                self.pos = start;
                self.graph.chase(start);
            }
        }
        frames
    }

    /// Fills a device buffer of interleaved frames with `channels` channels each.
    pub fn fill<T: Copy>(&mut self, data: &mut [T], channels: usize, convert: impl Fn(f32) -> T) {
        while let Ok(command) = self.commands.pop() {
            self.handle(command);
        }
        let channels = channels.max(1);
        let mut peak = [0.0f32; 2];
        let mut total = 0u64;
        for chunk in data.chunks_mut(MAX_BLOCK * channels) {
            let mut done = 0;
            let want = chunk.len() / channels;
            while done < want {
                let frames = self.block(want - done);
                let rendered = &self.scratch[..frames * 2];
                let target = &mut chunk[done * channels..(done + frames) * channels];
                for (out, stereo) in target.chunks_exact_mut(channels).zip(rendered.chunks_exact(2)) {
                    let left = sanitize(stereo[0]);
                    let right = sanitize(stereo[1]);
                    peak[0] = peak[0].max(left.abs());
                    peak[1] = peak[1].max(right.abs());
                    if channels == 1 {
                        out[0] = convert((left + right) * 0.5);
                    } else {
                        out[0] = convert(left);
                        out[1] = convert(right);
                        for extra in &mut out[2..] {
                            *extra = convert(0.0);
                        }
                    }
                }
                done += frames;
            }
            total += want as u64;
        }

        let shared = &self.shared;
        shared.tick.store(self.graph.tick_at(self.pos).to_bits(), Relaxed);
        shared.playing.store(self.playing, Relaxed);
        for (slot, value) in shared.peak.iter().zip(peak) {
            // Positive floats order like their bit patterns.
            slot.fetch_max(value.to_bits(), Relaxed);
        }
        shared.callbacks.fetch_add(1, Relaxed);
        shared.frames.fetch_add(total, Relaxed);
    }
}

/// The last line of defence before the DAC: no NaN, nothing beyond full scale.
#[inline]
fn sanitize(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}
