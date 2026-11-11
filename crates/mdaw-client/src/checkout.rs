//! The client's side of checkouts: rendering the samples of a commit, and keeping the work of
//! a checkout on disk.

use anyhow::{Context, Result};
use imbl::OrdMap;
use mdaw_dsp::render_offline;
use mdaw_engine::flac;
use mdaw_model::{
    Op, Project, TempoId, TempoPoint, TickRange, TrackContent, TrackId, TrackShell, affected_ranges, apply,
    apply_all, listening_range,
};
use mdaw_proto::{Sample, SampleKind, Superseded, blob_hash};
use mdaw_sync::Workspace;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Changelog samples are rendered at this rate, whatever the output device runs at.
pub const SAMPLE_RATE: u32 = 48_000;

/// A rendered sample, ready to play here and to upload.
#[derive(Clone)]
pub struct Rendered {
    pub sample: Sample,
    /// Interleaved stereo at [`SAMPLE_RATE`].
    pub audio: Arc<[f32]>,
    pub flac: Arc<[u8]>,
}

/// Everything a commit dialog needs, worked out off the UI thread.
#[derive(Clone)]
pub struct CommitPlan {
    pub track: TrackId,
    pub ops: Vec<Op>,
    /// Where the track changed.
    pub ranges: Vec<TickRange>,
    /// The tempo changed under the track since the checkout began.
    pub conflict: bool,
    /// Ops that put the tempo back to what it was at checkout.
    pub restore_tempo: Vec<Op>,
    /// The samples for committing as is.
    pub as_is: Vec<Rendered>,
    /// The samples for committing with the old tempo restored. Empty without a conflict.
    pub restored: Vec<Rendered>,
}

impl CommitPlan {
    /// The samples to listen to before deciding: those of committing as is, each sound once.
    /// With a conflict that is the track alone, and the whole project with the old and with
    /// the current tempo.
    pub fn to_listen(&self) -> Vec<&Rendered> {
        let keep = distinct(&self.as_is.iter().map(|r| r.sample).collect::<Vec<_>>());
        self.as_is.iter().filter(|r| keep.contains(&r.sample)).collect()
    }
}

/// The samples worth a button: when two sound the same (a project of one track, or the old
/// tempo restored), only the one with the more telling name.
pub fn distinct(samples: &[Sample]) -> Vec<Sample> {
    let rank = |kind: SampleKind| match kind {
        SampleKind::ProjectBase | SampleKind::ProjectCurrent => 2,
        SampleKind::Solo | SampleKind::Before => 1,
        SampleKind::InContext => 0,
    };
    let mut out: Vec<Sample> = Vec::new();
    for sample in samples {
        match out.iter_mut().find(|o| o.blob == sample.blob) {
            Some(same) if rank(sample.kind) > rank(same.kind) => *same = *sample,
            Some(_) => {}
            None => out.push(*sample),
        }
    }
    out
}

/// Works out the commit of `workspace` against `confirmed`, the document as the server has it.
pub fn plan_commit(confirmed: &Project, workspace: &Workspace) -> CommitPlan {
    let track = workspace.track;
    let ops = workspace.journal.clone();
    let mut current = confirmed.clone();
    apply_all(&mut current, &ops);

    let changed = affected_ranges(confirmed, &ops)
        .into_iter()
        .filter(|a| a.track == track)
        .find_map(|a| a.range);
    let content = current.track(track).map(|t| t.content.clone());
    let listen = content.as_deref().and_then(|c| listening_range(c, changed));
    let extent = content.as_deref().and_then(TrackContent::extent);

    let conflict = tempo_differs(&workspace.base_tempo, &confirmed.tempo, extent);
    let restore_tempo = if conflict {
        tempo_fix(&confirmed.tempo, &workspace.base_tempo)
    } else {
        Vec::new()
    };
    let mut restored_doc = current.clone();
    apply_all(&mut restored_doc, &restore_tempo);

    // Other tracks can be heard in context only if this client can render all of them.
    let context = current
        .tracks
        .iter()
        .all(|(id, t)| *id == track || !t.content.has_clap_device());

    let mut as_is = Vec::new();
    let mut restored = Vec::new();
    if let Some(range) = listen {
        let solo = render(&current, Some(track), range, SampleKind::Solo);
        as_is.push(solo);
        let in_context = context.then(|| render(&current, None, range, SampleKind::InContext));
        as_is.extend(in_context.clone());
        if conflict {
            let solo_base = render(&restored_doc, Some(track), range, SampleKind::Solo);
            restored.push(solo_base);
            if context {
                let base = render(&restored_doc, None, range, SampleKind::InContext);
                let project_base = relabel(&base, SampleKind::ProjectBase);
                let project_current =
                    relabel(in_context.as_ref().expect("rendered"), SampleKind::ProjectCurrent);
                restored.push(base);
                restored.push(project_base.clone());
                restored.push(project_current.clone());
                as_is.push(project_base);
                as_is.push(project_current);
            }
        }
    }
    CommitPlan {
        track,
        ops,
        ranges: changed.into_iter().collect(),
        conflict,
        restore_tempo,
        as_is,
        restored,
    }
}

/// The track as it sounded with an edit that was displaced, after `event_doc`: the document
/// right after the displacing event.
pub fn render_before(event_doc: &Project, lost: &Superseded) -> Option<Rendered> {
    let track = lost.entity.track()?;
    let mut before = event_doc.clone();
    let ranges = affected_ranges(&before, std::slice::from_ref(&lost.lost));
    apply(&mut before, &lost.lost)?;
    let changed = ranges
        .into_iter()
        .find(|a| a.track == track)
        .and_then(|a| a.range);
    let range = listening_range(&before.track(track)?.content, changed)?;
    Some(render(&before, Some(track), range, SampleKind::Before))
}

fn render(project: &Project, track: Option<TrackId>, range: TickRange, kind: SampleKind) -> Rendered {
    let only = track.map(|t| [t]);
    let audio = render_offline(project, only.as_ref().map(|t| &t[..]), range, SAMPLE_RATE);
    let bytes = flac::encode(&audio, SAMPLE_RATE).expect("encoding rendered audio");
    let ms = (audio.len() as u64 / 2 * 1000 / SAMPLE_RATE as u64) as u32;
    Rendered {
        sample: Sample {
            kind,
            blob: blob_hash(&bytes),
            range,
            ms,
        },
        audio: audio.into(),
        flac: bytes.into(),
    }
}

fn relabel(rendered: &Rendered, kind: SampleKind) -> Rendered {
    let mut out = rendered.clone();
    out.sample.kind = kind;
    out
}

/// True when the two tempo maps make the music inside `extent` run differently. A change
/// before it only moves the whole track in time, which every track shares.
pub fn tempo_differs(
    base: &OrdMap<TempoId, TempoPoint>,
    current: &OrdMap<TempoId, TempoPoint>,
    extent: Option<TickRange>,
) -> bool {
    let Some(extent) = extent else {
        return false;
    };
    let (a, b) = (
        mdaw_model::TempoMap::compile(base),
        mdaw_model::TempoMap::compile(current),
    );
    let ticks = base
        .values()
        .chain(current.values())
        .map(|p| p.tick)
        .filter(|t| *t > extent.start && *t < extent.end)
        .chain([extent.start]);
    ticks.into_iter().any(|t| a.milli_bpm_at(t) != b.milli_bpm_at(t))
}

/// Ops that turn the tempo points `current` into `wanted`.
pub fn tempo_fix(current: &OrdMap<TempoId, TempoPoint>, wanted: &OrdMap<TempoId, TempoPoint>) -> Vec<Op> {
    let mut ops = Vec::new();
    for id in current.keys().filter(|id| !wanted.contains_key(*id)) {
        ops.push(Op::TempoRemove { id: *id });
    }
    for (id, point) in wanted {
        if current.get(id) != Some(point) {
            ops.push(Op::TempoPut {
                id: *id,
                point: *point,
            });
        }
    }
    ops
}

/// The work of a checkout, as kept on disk while it lasts.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
struct JournalFile {
    track: TrackId,
    base_tempo: Vec<(TempoId, TempoPoint)>,
    journal: Vec<Op>,
}

/// What is left of a checkout someone else ended: the track as its owner had made it.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Fork {
    /// The track that was checked out.
    pub track: TrackId,
    pub shell: TrackShell,
    pub content: TrackContent,
    /// Milliseconds since the Unix epoch when it was saved.
    pub saved: u64,
    #[serde(skip)]
    pub path: PathBuf,
}

/// Where one project's checkouts and forks are kept, inside the profile.
#[derive(Clone, Debug)]
pub struct Files {
    dir: PathBuf,
}

impl Files {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn journal_path(&self, track: TrackId) -> PathBuf {
        self.dir
            .join("checkouts")
            .join(format!("{:016x}.journal", track.0.0))
    }

    pub fn save_journal(&self, workspace: &Workspace) -> Result<()> {
        let file = JournalFile {
            track: workspace.track,
            base_tempo: workspace.base_tempo.iter().map(|(k, v)| (*k, *v)).collect(),
            journal: workspace.journal.clone(),
        };
        write_atomic(&self.journal_path(workspace.track), &postcard::to_stdvec(&file)?)
    }

    /// The journal of `track` saved earlier, with the tempo the checkout began with.
    pub fn load_journal(&self, track: TrackId) -> Option<(Vec<Op>, OrdMap<TempoId, TempoPoint>)> {
        let bytes = std::fs::read(self.journal_path(track)).ok()?;
        let file: JournalFile = postcard::from_bytes(&bytes).ok()?;
        (file.track == track).then(|| (file.journal, file.base_tempo.into_iter().collect()))
    }

    pub fn remove_journal(&self, track: TrackId) {
        let _ = std::fs::remove_file(self.journal_path(track));
    }

    pub fn save_fork(&self, fork: &Fork) -> Result<PathBuf> {
        let path = self.dir.join("forks").join(format!("{}.fork", fork.saved));
        write_atomic(&path, &postcard::to_stdvec(fork)?)?;
        Ok(path)
    }

    /// The forks saved so far, oldest first.
    pub fn forks(&self) -> Vec<Fork> {
        let Ok(entries) = std::fs::read_dir(self.dir.join("forks")) else {
            return Vec::new();
        };
        let mut forks: Vec<Fork> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "fork"))
            .filter_map(|path| {
                let mut fork: Fork = postcard::from_bytes(&std::fs::read(&path).ok()?).ok()?;
                fork.path = path;
                Some(fork)
            })
            .collect();
        forks.sort_by_key(|f| f.saved);
        forks
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("a file without a directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let part = path.with_extension("part");
    std::fs::write(&part, bytes).with_context(|| format!("writing {}", part.display()))?;
    std::fs::rename(&part, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdaw_model::{EntityId, Tick};

    fn tempo(points: &[(u64, i64, u32)]) -> OrdMap<TempoId, TempoPoint> {
        points
            .iter()
            .map(|&(id, bar, bpm)| {
                let point = TempoPoint {
                    tick: Tick::bars(bar),
                    milli_bpm: bpm * 1000,
                };
                (TempoId(EntityId(id)), point)
            })
            .collect()
    }

    #[test]
    fn only_tempo_changes_under_the_track_conflict() {
        let base = tempo(&[(1, 0, 120)]);
        let track = Some(TickRange::new(Tick::bars(4), Tick::bars(8)));
        assert!(!tempo_differs(&base, &base, track));
        assert!(tempo_differs(&base, &tempo(&[(1, 0, 140)]), track));
        assert!(tempo_differs(&base, &tempo(&[(1, 0, 120), (2, 6, 90)]), track));
        // After the track ends, or for a silent track, nothing changes for it.
        assert!(!tempo_differs(&base, &tempo(&[(1, 0, 120), (2, 9, 90)]), track));
        assert!(!tempo_differs(&base, &tempo(&[(1, 0, 140)]), None));
    }

    #[test]
    fn the_fix_turns_the_current_tempo_into_the_old_one() {
        let base = tempo(&[(1, 0, 120), (3, 2, 100)]);
        let current = tempo(&[(1, 0, 140), (2, 6, 90)]);
        let mut project = Project {
            tempo: current.clone(),
            ..Project::default()
        };
        apply_all(&mut project, &tempo_fix(&current, &base));
        assert_eq!(project.tempo, base);
    }

    #[test]
    fn journals_and_forks_round_trip_through_files() {
        let dir = tempfile::tempdir().unwrap();
        let files = Files::new(dir.path().to_path_buf());
        let track = TrackId(EntityId(5));
        let workspace = Workspace {
            track,
            journal: vec![Op::TrackSetShell {
                id: track,
                prop: mdaw_model::ShellProp::Color(7),
            }],
            base_tempo: tempo(&[(1, 0, 99)]),
            committing: false,
        };
        files.save_journal(&workspace).unwrap();
        let (journal, base_tempo) = files.load_journal(track).unwrap();
        assert_eq!(
            (journal, base_tempo),
            (workspace.journal.clone(), workspace.base_tempo)
        );
        files.remove_journal(track);
        assert!(files.load_journal(track).is_none());

        let fork = Fork {
            track,
            shell: TrackShell {
                name: "x".into(),
                color: 1,
            },
            content: TrackContent::default(),
            saved: 42,
            path: PathBuf::new(),
        };
        let path = files.save_fork(&fork).unwrap();
        let forks = files.forks();
        assert_eq!(forks.len(), 1);
        assert_eq!((forks[0].saved, &forks[0].path), (42, &path));
        assert_eq!(forks[0].track, track);
    }
}
