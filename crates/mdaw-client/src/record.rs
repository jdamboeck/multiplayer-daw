//! Turns notes played during playback into an edit.

use std::collections::{BTreeMap, HashMap};

use imbl::OrdMap;
use mdaw_model::{Clip, IdGen, Note, Op, Project, TICKS_PER_BAR, Tick, TickRange, TrackId};

use crate::live::Played;

/// The shortest note a take writes, in ticks: a note let go at once still shows.
const MIN_DUR: i64 = 30;

/// A note of a take, in ticks of the timeline.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TakeNote {
    pub track: TrackId,
    pub start: f64,
    pub end: f64,
    pub pitch: u8,
    pub vel: u8,
}

/// The notes played during one run of the transport.
#[derive(Default)]
pub struct Take {
    notes: Vec<TakeNote>,
    /// Notes that sound, by pitch: their track, start and velocity.
    open: HashMap<u8, (TrackId, f64, u8)>,
    /// The latest place of the playhead a note was played at or the take was told of.
    last: f64,
    /// The loop of the transport, for notes that wrap around it.
    loop_range: Option<TickRange>,
}

impl Take {
    pub fn is_empty(&self) -> bool {
        self.notes.is_empty() && self.open.is_empty()
    }

    /// Notes played so far, finished or not.
    pub fn len(&self) -> usize {
        self.notes.len() + self.open.len()
    }

    /// Where the playhead is, and the loop it runs in.
    pub fn playhead(&mut self, tick: f64, loop_range: Option<TickRange>) {
        self.last = tick;
        self.loop_range = loop_range;
    }

    /// Takes in a note played at `tick`.
    pub fn add(&mut self, played: &Played, tick: f64) {
        self.last = tick;
        if let Some((track, start, vel)) = self.open.remove(&played.pitch) {
            self.close(track, start, tick, played.pitch, vel);
        }
        if played.vel > 0 {
            self.open.insert(played.pitch, (played.track, tick, played.vel));
        }
    }

    fn close(&mut self, track: TrackId, start: f64, end: f64, pitch: u8, vel: u8) {
        // The playhead wrapped around the loop while the note sounded: it lasts to the loop's
        // end.
        let end = match self.loop_range {
            Some(range) if end < start && start < range.end.0 as f64 => range.end.0 as f64,
            _ => end,
        };
        self.notes.push(TakeNote {
            track,
            start,
            end,
            pitch,
            vel,
        });
    }

    /// Ends the take: notes still held end where the playhead was last.
    pub fn finish(mut self) -> Vec<TakeNote> {
        let open: Vec<_> = self.open.drain().collect();
        for (pitch, (track, start, vel)) in open {
            let end = self.last;
            self.close(track, start, end, pitch, vel);
        }
        self.notes
    }
}

/// The ops that put `notes` into `project`, as one edit: per track into the clip that spans all
/// of them, or else into a new clip of whole bars.
pub fn take_ops(project: &Project, ids: &mut IdGen, notes: &[TakeNote]) -> Vec<Op> {
    let mut by_track: BTreeMap<TrackId, Vec<(i64, i64, u8, u8)>> = BTreeMap::new();
    for n in notes {
        let start = (n.start.round() as i64).max(0);
        let dur = (n.end.round() as i64 - start).max(MIN_DUR);
        by_track
            .entry(n.track)
            .or_default()
            .push((start, dur, n.pitch.min(127), n.vel.clamp(1, 127)));
    }
    let mut ops = Vec::new();
    for (track, notes) in by_track {
        let Some(content) = project.track(track).map(|t| &t.content) else {
            continue;
        };
        let first = notes.iter().map(|n| n.0).min().unwrap_or(0);
        let last = notes.iter().map(|n| n.0 + n.1).max().unwrap_or(0);
        let into = content
            .clips
            .iter()
            .find(|(_, c)| c.start.0 <= first && last <= c.start.0 + c.length.0);
        let relative = |offset: i64, (start, dur, pitch, vel): (i64, i64, u8, u8)| Note {
            start: Tick(start - offset),
            dur: Tick(dur),
            pitch,
            vel,
        };
        match into {
            Some((&clip, existing)) => {
                for note in notes {
                    ops.push(Op::NoteAdd {
                        track,
                        clip,
                        id: ids.note(),
                        note: relative(existing.start.0, note),
                    });
                }
            }
            None => {
                let start = first.div_euclid(TICKS_PER_BAR) * TICKS_PER_BAR;
                let end = (last + TICKS_PER_BAR - 1).div_euclid(TICKS_PER_BAR) * TICKS_PER_BAR;
                let notes: OrdMap<_, _> = notes
                    .into_iter()
                    .map(|n| (ids.note(), relative(start, n)))
                    .collect();
                ops.push(Op::ClipAdd {
                    track,
                    id: ids.clip(),
                    clip: Clip {
                        start: Tick(start),
                        length: Tick((end - start).max(TICKS_PER_BAR)),
                        name: "Take".into(),
                        notes,
                    },
                });
            }
        }
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdaw_model::{ActorId, apply_all};

    fn played(pitch: u8, vel: u8, track: TrackId) -> Played {
        Played {
            micros: 0,
            track,
            pitch,
            vel,
        }
    }

    #[test]
    fn a_take_goes_into_a_new_clip_of_whole_bars_or_into_the_clip_it_fits() {
        let mut ids = IdGen::new(ActorId(5));
        let mut project = Project::default();
        let add = crate::session::new_track(&mut ids, 0, None);
        let Op::TrackAdd { id: track, .. } = add else {
            unreachable!()
        };
        apply_all(&mut project, &[add]);

        let mut take = Take::default();
        take.add(&played(60, 100, track), 2000.0);
        take.add(&played(64, 90, track), 2400.4);
        take.add(&played(60, 0, track), 2900.0);
        take.playhead(4000.0, None);
        let notes = take.finish();
        let ops = take_ops(&project, &mut ids, &notes);
        let [Op::ClipAdd { clip, .. }] = ops.as_slice() else {
            panic!("{ops:?}")
        };
        assert_eq!(clip.start, Tick::bars(0));
        assert_eq!(clip.length, Tick::bars(2));
        let mut got: Vec<_> = clip
            .notes
            .values()
            .map(|n| (n.start.0, n.dur.0, n.pitch, n.vel))
            .collect();
        got.sort();
        // The note still held at the end lasts to where the playhead was.
        assert_eq!(got, [(2000, 900, 60, 100), (2400, 1600, 64, 90)]);
        apply_all(&mut project, &ops);

        // A second take inside that clip adds to it.
        let mut take = Take::default();
        take.add(&played(67, 80, track), 100.0);
        take.add(&played(67, 0, track), 105.0);
        let ops = take_ops(&project, &mut ids, &take.finish());
        let [Op::NoteAdd { note, .. }] = ops.as_slice() else {
            panic!("{ops:?}")
        };
        assert_eq!((note.start.0, note.dur.0), (100, MIN_DUR));
    }

    #[test]
    fn a_note_held_across_the_end_of_the_loop_lasts_to_it() {
        let track = IdGen::new(ActorId(5)).track();
        let mut take = Take::default();
        take.playhead(0.0, Some(TickRange::new(Tick::bars(1), Tick::bars(2))));
        take.add(&played(60, 100, track), 7000.0);
        take.add(&played(60, 0, track), 4000.0);
        assert_eq!(take.finish()[0].end, Tick::bars(2).0 as f64);
    }
}
