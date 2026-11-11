//! The part of the engine that runs inside the audio callback. Nothing here allocates, frees,
//! locks or blocks.

use mdaw_dsp::{Graph, MAX_BLOCK};
use mdaw_model::TrackId;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering::Relaxed};

/// While following, a playhead further than this from where it should be is put right, once
/// it has been so for [`DRIFT_PATIENCE`] buffers in a row: a single late callback is not drift.
const MAX_DRIFT_MICROS: i64 = 20_000;
const DRIFT_PATIENCE: u32 = 3;
/// Right after a new anchor the playhead is put right at once, unless it is as good as there:
/// this is where seeks arrive, and nobody wants to wait for those.
const FRESH_DRIFT_MICROS: i64 = 2_000;

/// The part of an audio callback's time that plugins in other processes may use, in percent of
/// the buffer's length. The rest is for everything else, and for the device.
const DEADLINE_PERCENT: u64 = 60;

/// A point of the timeline pinned to a moment of the engine's clock.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Anchor {
    pub tick: f64,
    /// On the clock that `fill` is given the buffers' play times on.
    pub micros: i64,
}

pub(crate) enum Command {
    /// Replace the graph. The old one travels back for disposal.
    SetGraph(Box<Graph>),
    /// Play freely from where the playhead is.
    Play,
    Stop,
    /// Play so that the playhead is at the anchor's tick at the anchor's time: wait there if
    /// that is still to come, otherwise be wherever that puts the playhead by now, and stay
    /// in step from then on. `fresh` marks an anchor that describes a new playback rather
    /// than a better estimate of the same one.
    Follow {
        anchor: Anchor,
        fresh: bool,
    },
    /// Stop following; what was following plays on freely.
    Unfollow,
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
    /// Play a piece of audio on its own, beside the transport: interleaved stereo at the
    /// engine's rate. `None` stops what plays. The control side keeps a reference to the audio
    /// until `Shared::preview_id` has moved past `id`, so the audio thread never frees it.
    Preview {
        id: u64,
        audio: Option<Arc<[f32]>>,
    },
}

/// A live note from any thread, through [`crate::LiveInput`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum LiveNote {
    On {
        track: TrackId,
        key: u64,
        pitch: u8,
        vel: u8,
    },
    Off {
        track: TrackId,
        key: u64,
    },
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
    /// Whether playback follows an anchor and has started.
    pub following: AtomicBool,
    /// While following: how far the playhead was ahead of where it should be at the start of
    /// the last buffer, in microseconds.
    pub drift_micros: AtomicI64,
    /// Jumps made to get back in step.
    pub resyncs: AtomicU64,
    /// The `id` of the last preview command taken.
    pub preview_id: AtomicU64,
    /// Frames of the preview played so far, or `u64::MAX` once it is over or stopped.
    pub preview_pos: AtomicU64,
    /// When the playhead in `tick` is heard, on the engine's clock.
    pub tick_heard_at: AtomicI64,
    /// The listener's own volume, as `f32` bits: applied to everything this engine outputs,
    /// and nowhere else.
    pub output_gain: AtomicU32,
}

pub(crate) struct RtCore {
    graph: Box<Graph>,
    commands: rtrb::Consumer<Command>,
    live: rtrb::Consumer<LiveNote>,
    garbage: rtrb::Producer<Box<Graph>>,
    shared: Arc<Shared>,
    playing: bool,
    /// Timeline position in samples.
    pos: i64,
    loop_ticks: Option<(f64, f64)>,
    loop_samples: Option<(i64, i64)>,
    follow: Option<Anchor>,
    /// The anchor has not been compared with the playhead yet.
    fresh: bool,
    /// Frames until a start that falls into the buffer being filled, or after it.
    start_in: Option<usize>,
    /// Buffers in a row that began with too much drift.
    adrift: u32,
    scratch: [f32; MAX_BLOCK * 2],
    preview: Option<Arc<[f32]>>,
    /// Next sample of `preview` to play.
    preview_at: usize,
}

impl RtCore {
    pub fn new(
        graph: Box<Graph>,
        commands: rtrb::Consumer<Command>,
        live: rtrb::Consumer<LiveNote>,
        garbage: rtrb::Producer<Box<Graph>>,
        shared: Arc<Shared>,
    ) -> Self {
        Self {
            graph,
            commands,
            live,
            garbage,
            shared,
            playing: false,
            pos: 0,
            loop_ticks: None,
            loop_samples: None,
            follow: None,
            fresh: false,
            start_in: None,
            adrift: 0,
            scratch: [0.0; MAX_BLOCK * 2],
            preview: None,
            preview_at: 0,
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
                let mut old = std::mem::replace(&mut self.graph, graph);
                // Plugins that did not move to the new graph stop here, on the audio thread.
                old.retire();
                self.update_loop();
                if let Err(rtrb::PushError::Full(old)) = self.garbage.push(old) {
                    // Freeing here would allocate-lock the audio thread. The control side
                    // limits outstanding graphs, so this does not happen; leak if it does.
                    std::mem::forget(old);
                }
            }
            Command::Play => {
                self.follow = None;
                self.start_in = None;
                self.start();
            }
            Command::Stop => {
                self.follow = None;
                self.start_in = None;
                self.halt();
            }
            Command::Follow { anchor, fresh } => {
                self.fresh |= fresh || self.follow.is_none();
                self.follow = Some(anchor);
                self.adrift = 0;
            }
            Command::Unfollow => {
                // Whoever followed is playing as far as the control side knows, even if the
                // start was still to come.
                if self.follow.take().is_some() {
                    self.start_in = None;
                    self.start();
                }
            }
            Command::Seek(tick) => self.jump(self.graph.sample_at(tick.max(0.0))),
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
            Command::Preview { id, audio } => {
                // Dropping the old audio frees nothing: the control side still holds it.
                self.preview = audio;
                self.preview_at = 0;
                self.shared.preview_id.store(id, Relaxed);
            }
        }
    }

    fn start(&mut self) {
        if !self.playing {
            self.playing = true;
            self.graph.chase(self.pos);
        }
    }

    fn halt(&mut self) {
        if self.playing {
            self.playing = false;
            self.graph.release_timeline_notes();
        }
    }

    fn jump(&mut self, pos: i64) {
        self.pos = pos;
        if self.playing {
            self.graph.release_timeline_notes();
            self.graph.chase(pos);
        }
    }

    fn frames(&self, micros: i64) -> i64 {
        (micros as f64 * self.graph.sample_rate() as f64 / 1e6).round() as i64
    }

    /// Where playback that passed `from` is `elapsed` frames later: it wraps at the end of
    /// the loop if it started before it, like [`RtCore::block`] does.
    fn ahead(&self, from: i64, elapsed: i64) -> i64 {
        let at = from.saturating_add(elapsed);
        match self.loop_samples {
            Some((start, end)) if from < end && at >= end => start + (at - start).rem_euclid(end - start),
            _ => at,
        }
    }

    /// Brings the transport in line with the anchor, for a buffer whose first frame is heard
    /// at `play_micros`.
    fn steer(&mut self, play_micros: i64) {
        self.start_in = None;
        let Some(anchor) = self.follow else {
            self.shared.following.store(false, Relaxed);
            return;
        };
        let at = self.graph.sample_at(anchor.tick.max(0.0));
        let lead = anchor.micros.saturating_sub(play_micros);
        if lead > 0 {
            // The start is still to come: wait at the anchor, and begin on the exact frame.
            self.halt();
            self.pos = at;
            self.start_in = Some(self.frames(lead) as usize);
            self.shared.following.store(false, Relaxed);
            return;
        }
        let expected = self.ahead(at, self.frames(-lead));
        if !self.playing {
            self.pos = expected;
            self.start();
            self.fresh = false;
            self.adrift = 0;
        }
        let mut drift = self.pos - expected;
        if let Some((start, end)) = self.loop_samples
            && (start..end).contains(&self.pos)
            && (start..end).contains(&expected)
        {
            // Inside the loop, the short way round is what counts.
            let len = end - start;
            drift = (drift + len / 2).rem_euclid(len) - len / 2;
        }
        let micros = (drift as f64 * 1e6 / self.graph.sample_rate() as f64) as i64;
        let limit = if self.fresh {
            FRESH_DRIFT_MICROS
        } else {
            MAX_DRIFT_MICROS
        };
        self.adrift = if micros.abs() > limit { self.adrift + 1 } else { 0 };
        if self.adrift >= DRIFT_PATIENCE || (self.fresh && self.adrift > 0) {
            self.jump(expected);
            self.adrift = 0;
            self.shared.resyncs.fetch_add(1, Relaxed);
            self.shared.drift_micros.store(0, Relaxed);
        } else {
            self.shared.drift_micros.store(micros, Relaxed);
        }
        self.fresh = false;
        self.shared.following.store(true, Relaxed);
    }

    /// Renders up to `max` frames into the scratch buffer and returns how many.
    fn block(&mut self, max: usize) -> usize {
        let mut frames = max.min(MAX_BLOCK);
        match self.start_in {
            Some(0) => {
                self.start_in = None;
                self.start();
            }
            Some(wait) => {
                frames = frames.min(wait);
                self.start_in = Some(wait - frames);
            }
            None => {}
        }
        let pos = self.playing.then_some(self.pos);
        if let (Some(pos), Some((_, end))) = (pos, self.loop_samples)
            && pos < end
        {
            frames = frames.min((end - pos) as usize);
        }
        self.graph.process(pos, &mut self.scratch[..frames * 2]);
        if let Some(preview) = &self.preview {
            let left = preview.len().saturating_sub(self.preview_at).min(frames * 2);
            for (out, sample) in self.scratch[..left]
                .iter_mut()
                .zip(&preview[self.preview_at..self.preview_at + left])
            {
                *out += sample;
            }
            self.preview_at += left;
            if self.preview_at >= preview.len() {
                self.preview = None;
            }
        }
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

    /// Fills a device buffer of interleaved frames with `channels` channels each. Its first
    /// frame is heard at `play_micros` on the clock that anchors are given on.
    pub fn fill<T: Copy>(
        &mut self,
        data: &mut [T],
        channels: usize,
        play_micros: i64,
        convert: impl Fn(f32) -> T,
    ) {
        while let Ok(command) = self.commands.pop() {
            self.handle(command);
        }
        while let Ok(note) = self.live.pop() {
            match note {
                LiveNote::On {
                    track,
                    key,
                    pitch,
                    vel,
                } => self.graph.note_on(track, key, pitch, vel),
                LiveNote::Off { track, key } => self.graph.note_off(track, key),
            }
        }
        self.steer(play_micros);
        let channels = channels.max(1);
        // Plugins in other processes may take part of the callback, together; whatever they
        // have not delivered by then goes without them.
        let frames = (data.len() / channels) as u64;
        let budget = frames * 1_000_000 * DEADLINE_PERCENT / 100 / self.graph.sample_rate().max(1) as u64;
        self.graph.set_deadline(Some(
            std::time::Instant::now() + std::time::Duration::from_micros(budget),
        ));
        let mut peak = [0.0f32; 2];
        let mut total = 0u64;
        let gain = f32::from_bits(self.shared.output_gain.load(Relaxed));
        for chunk in data.chunks_mut(MAX_BLOCK * channels) {
            let mut done = 0;
            let want = chunk.len() / channels;
            while done < want {
                let frames = self.block(want - done);
                let rendered = &self.scratch[..frames * 2];
                let target = &mut chunk[done * channels..(done + frames) * channels];
                for (out, stereo) in target.chunks_exact_mut(channels).zip(rendered.as_chunks::<2>().0) {
                    let left = sanitize(stereo[0] * gain);
                    let right = sanitize(stereo[1] * gain);
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
        // The playhead as published is where the next buffer starts: heard after this one.
        let rate = self.graph.sample_rate().max(1) as i64;
        let heard = play_micros + total as i64 * 1_000_000 / rate;
        shared.tick_heard_at.store(heard, Relaxed);
        shared.playing.store(self.playing, Relaxed);
        for (slot, value) in shared.peak.iter().zip(peak) {
            // Positive floats order like their bit patterns.
            slot.fetch_max(value.to_bits(), Relaxed);
        }
        let preview_pos = match self.preview {
            Some(_) => (self.preview_at / 2) as u64,
            None => u64::MAX,
        };
        shared.preview_pos.store(preview_pos, Relaxed);
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
