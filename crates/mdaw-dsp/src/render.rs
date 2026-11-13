use crate::compile::Compiler;
use crate::external::{NoPlugins, PluginSource};
use crate::graph::{Graph, MAX_BLOCK};
use mdaw_model::{Project, Tick, TickRange, TrackId};

/// Sound before the range that is rendered and thrown away, so that envelopes and release tails
/// reaching into the range are right. Notes held since before the pre-roll are started at its
/// beginning.
pub const PRE_ROLL_SECONDS: u32 = 2;
/// Extra time after the range for releases to ring out.
pub const TAIL_SECONDS: u32 = 1;
/// Silence external devices get to settle in before a render, in milliseconds: a plugin just
/// made and loaded with a state may still be getting there in its first moments.
pub const WARM_UP_MILLIS: u32 = 500;

/// Renders part of a project to interleaved stereo `f32`.
///
/// `tracks: None` renders all tracks. The result covers `range` followed by a one-second tail in
/// which no new notes start. For built-in devices the output is bit-identical on every machine.
pub fn render_offline(
    project: &Project,
    tracks: Option<&[TrackId]>,
    range: TickRange,
    sample_rate: u32,
) -> Vec<f32> {
    render_with_block(project, tracks, range, sample_rate, 256)
}

/// As [`render_offline`], with plugins from `source`.
pub fn render_offline_with(
    project: &Project,
    tracks: Option<&[TrackId]>,
    range: TickRange,
    sample_rate: u32,
    source: &mut dyn PluginSource,
) -> Vec<f32> {
    let mut render = OfflineRender::new(project, tracks, range, sample_rate, 256, source);
    while !render.step(usize::MAX) {}
    render.finish()
}

/// As [`render_offline`], with an explicit block size. The output does not depend on it.
pub fn render_with_block(
    project: &Project,
    tracks: Option<&[TrackId]>,
    range: TickRange,
    sample_rate: u32,
    block: usize,
) -> Vec<f32> {
    let mut render = OfflineRender::new(project, tracks, range, sample_rate, block, &mut NoPlugins);
    while !render.step(usize::MAX) {}
    render.finish()
}

/// An offline render that can be done a piece at a time, as when the devices must run on the
/// thread that also draws the screen.
pub struct OfflineRender {
    graph: Graph,
    block: usize,
    pos: i64,
    start: i64,
    end: i64,
    /// Frames of tail still to render, once the range is done.
    tail: usize,
    /// The timeline's notes have been let go, for the tail.
    released: bool,
    out: Vec<f32>,
    scratch: Vec<f32>,
}

impl OfflineRender {
    pub fn new(
        project: &Project,
        tracks: Option<&[TrackId]>,
        range: TickRange,
        sample_rate: u32,
        block: usize,
        source: &mut dyn PluginSource,
    ) -> Self {
        let block = block.clamp(1, MAX_BLOCK);
        let mut graph = Compiler::new(sample_rate).build_with(project, tracks, source);
        let start = graph.sample_at(range.start.max(Tick::ZERO).0 as f64);
        let end = graph.sample_at(range.end.max(Tick::ZERO).0 as f64).max(start);
        let pre_roll = start.min((PRE_ROLL_SECONDS * sample_rate) as i64);
        let tail = (TAIL_SECONDS * sample_rate) as usize;
        if graph.has_external() {
            let mut left = (WARM_UP_MILLIS * sample_rate / 1000) as usize;
            let mut scratch = vec![0.0f32; block * 2];
            while left > 0 {
                let n = left.min(block);
                graph.process(None, &mut scratch[..n * 2]);
                left -= n;
            }
        }
        let pos = start - pre_roll;
        graph.chase(pos);
        Self {
            graph,
            block,
            pos,
            start,
            end,
            tail,
            released: false,
            out: Vec::with_capacity(((end - start) as usize + tail) * 2),
            scratch: vec![0.0f32; block * 2],
        }
    }

    /// Renders up to about `frames` more frames. True once everything is rendered.
    pub fn step(&mut self, frames: usize) -> bool {
        let mut budget = frames;
        while budget > 0 {
            if self.pos < self.end {
                // Stop exactly at `start` once, so the kept part begins on a block boundary.
                let limit = if self.pos < self.start {
                    self.start
                } else {
                    self.end
                };
                let n = ((limit - self.pos) as usize).min(self.block);
                let buf = &mut self.scratch[..n * 2];
                self.graph.process(Some(self.pos), buf);
                if self.pos >= self.start {
                    self.out.extend_from_slice(buf);
                }
                self.pos += n as i64;
                budget = budget.saturating_sub(n);
            } else if self.tail > 0 {
                if !self.released {
                    self.graph.release_timeline_notes();
                    self.released = true;
                }
                let n = self.tail.min(self.block);
                let buf = &mut self.scratch[..n * 2];
                self.graph.process(None, buf);
                self.out.extend_from_slice(buf);
                self.tail -= n;
                budget = budget.saturating_sub(n);
            } else {
                return true;
            }
        }
        self.pos >= self.end && self.tail == 0
    }

    /// How far along it is, from 0 to 1.
    pub fn progress(&self) -> f32 {
        let total = (self.end - self.start) as f32 + (TAIL_SECONDS * self.graph.sample_rate()) as f32;
        (self.out.len() / 2) as f32 / total.max(1.0)
    }

    /// The rendered audio. Call once [`OfflineRender::step`] returned true. The graph's devices
    /// are told they are done.
    pub fn finish(mut self) -> Vec<f32> {
        self.graph.retire();
        self.out
    }
}
