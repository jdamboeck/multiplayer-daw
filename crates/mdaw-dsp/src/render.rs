use crate::compile::Compiler;
use crate::graph::MAX_BLOCK;
use mdaw_model::{Project, Tick, TickRange, TrackId};

/// Sound before the range that is rendered and thrown away, so that envelopes and release tails
/// reaching into the range are right. Notes held since before the pre-roll are started at its
/// beginning.
pub const PRE_ROLL_SECONDS: u32 = 2;
/// Extra time after the range for releases to ring out.
pub const TAIL_SECONDS: u32 = 1;

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

/// As [`render_offline`], with an explicit block size. The output does not depend on it.
pub fn render_with_block(
    project: &Project,
    tracks: Option<&[TrackId]>,
    range: TickRange,
    sample_rate: u32,
    block: usize,
) -> Vec<f32> {
    let block = block.clamp(1, MAX_BLOCK);
    let mut graph = Compiler::new(sample_rate).build(project, tracks);
    let start = graph.sample_at(range.start.max(Tick::ZERO).0 as f64);
    let end = graph.sample_at(range.end.max(Tick::ZERO).0 as f64).max(start);
    let pre_roll = start.min((PRE_ROLL_SECONDS * sample_rate) as i64);
    let tail = (TAIL_SECONDS * sample_rate) as i64;

    let mut out = Vec::with_capacity(((end - start + tail) * 2) as usize);
    let mut scratch = vec![0.0f32; block * 2];

    let mut pos = start - pre_roll;
    graph.chase(pos);
    while pos < end {
        // Stop exactly at `start` once, so the kept part begins on a block boundary.
        let limit = if pos < start { start } else { end };
        let frames = ((limit - pos) as usize).min(block);
        let buf = &mut scratch[..frames * 2];
        graph.process(Some(pos), buf);
        if pos >= start {
            out.extend_from_slice(buf);
        }
        pos += frames as i64;
    }

    graph.release_timeline_notes();
    let mut left = tail as usize;
    while left > 0 {
        let frames = left.min(block);
        let buf = &mut scratch[..frames * 2];
        graph.process(None, buf);
        out.extend_from_slice(buf);
        left -= frames;
    }
    out
}
