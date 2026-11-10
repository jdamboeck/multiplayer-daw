use crate::apply::apply;
use crate::doc::*;
use crate::ids::*;
use crate::op::Op;
use crate::time::{TICKS_PER_BAR, Tick, TickRange};

/// A track touched by a batch of ops.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Affected {
    pub track: TrackId,
    /// Absolute span of the notes and clips that changed. `None` when only non-temporal state
    /// changed (mix, devices).
    pub range: Option<TickRange>,
}

/// Which tracks a batch of ops touches, and where. Client and server both call this with the
/// state before the batch, so they agree on what a changelog entry covers.
///
/// Tracks come back in the order they were first touched. Project-scope ops (tempo, track order,
/// names) touch no track.
pub fn affected_ranges(before: &Project, ops: &[Op]) -> Vec<Affected> {
    let mut project = before.clone();
    let mut out: Vec<Affected> = Vec::new();
    let mut touch = |track: TrackId, range: Option<TickRange>| {
        let range = range.filter(|r| !r.is_empty());
        match out.iter_mut().find(|a| a.track == track) {
            Some(entry) => {
                entry.range = match (entry.range, range) {
                    (Some(a), Some(b)) => Some(a.union(b)),
                    (a, b) => a.or(b),
                }
            }
            None => out.push(Affected { track, range }),
        }
    };

    for op in ops {
        let span_before = span(&project, op);
        if apply(&mut project, op).is_none() {
            continue;
        }
        let span_after = span(&project, op);
        if let Some(track) = op.entity().track() {
            touch(track, span_before);
            touch(track, span_after);
        }
    }
    out
}

/// Absolute range covered by the entity `op` addresses, in the given state.
fn span(project: &Project, op: &Op) -> Option<TickRange> {
    match op {
        Op::TrackAdd { id, .. } | Op::TrackRemove { id } => project.track(*id)?.content.extent(),
        Op::ClipAdd { track, id, .. } | Op::ClipRemove { track, id } | Op::ClipSet { track, id, .. } => {
            Some(project.track(*track)?.content.clips.get(id)?.range())
        }
        Op::NoteAdd { track, clip, id, .. }
        | Op::NoteRemove { track, clip, id }
        | Op::NoteSet { track, clip, id, .. } => {
            let clip = project.track(*track)?.content.clips.get(clip)?;
            let note = clip.notes.get(id)?;
            let start = clip.start + note.start;
            Some(TickRange::new(start, start + note.dur))
        }
        _ => None,
    }
}

/// The part of a track a changelog sample should play.
///
/// With a changed range: that range grown to whole bars, at most `MAX_BARS` from its start.
/// Without one (a mix or device change): four bars starting at the first bar that has a note.
/// `None` when there is nothing to hear.
pub fn listening_range(content: &TrackContent, range: Option<TickRange>) -> Option<TickRange> {
    const MAX_BARS: i64 = 8;
    const DEFAULT_BARS: i64 = 4;
    match range {
        Some(range) => {
            let snapped = range.snap_to_bars();
            let cap = snapped.start + Tick::bars(MAX_BARS);
            Some(TickRange::new(snapped.start, snapped.end.min(cap)))
        }
        None => {
            let first_note = content
                .clips
                .values()
                .flat_map(|clip| {
                    clip.notes
                        .values()
                        .filter(|n| n.start < clip.length)
                        .map(|n| clip.start + n.start)
                })
                .min()?;
            let start = Tick(first_note.0.div_euclid(TICKS_PER_BAR) * TICKS_PER_BAR);
            Some(TickRange::new(start, start + Tick::bars(DEFAULT_BARS)))
        }
    }
}
