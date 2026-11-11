//! The graph with devices it does not know: plugins, through `ExternalDevice`, and stems.

use mdaw_dsp::{BlockTime, Compiler, ExternalDevice, PluginRef, PluginSource};
use mdaw_model::*;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

const RATE: u32 = 48_000;

/// What the fakes saw, shared with the test.
#[derive(Default)]
struct Log {
    made: AtomicUsize,
    retired: AtomicUsize,
    notes: Mutex<Vec<(u64, u8)>>,
}

/// An instrument that plays a constant 0.1 on the left and 0.2 on the right while a note
/// sounds, so that its output can be counted.
struct Fake {
    log: Arc<Log>,
    sounding: usize,
}

impl ExternalDevice for Fake {
    fn note_on(&mut self, key: u64, pitch: u8, _vel: u8) {
        self.sounding += 1;
        self.log.notes.lock().unwrap().push((key, pitch));
    }
    fn note_off(&mut self, _key: u64) {
        self.sounding = self.sounding.saturating_sub(1);
    }
    fn release_where(&mut self, _filter: &dyn Fn(u64) -> bool) {
        self.sounding = 0;
    }
    fn process(&mut self, left: &mut [f32], right: &mut [f32], _time: &BlockTime) {
        if self.sounding > 0 {
            left.iter_mut().for_each(|s| *s += 0.1);
            right.iter_mut().for_each(|s| *s += 0.2);
        }
    }
    fn retire(&mut self) {
        self.log.retired.fetch_add(1, Relaxed);
    }
}

struct Source {
    log: Arc<Log>,
    installed: bool,
    stem: Option<Arc<[f32]>>,
}

impl PluginSource for Source {
    fn device(&mut self, _: TrackId, _: DeviceId, plugin: PluginRef<'_>) -> Option<Box<dyn ExternalDevice>> {
        assert_eq!(plugin.id, "org.example.fake");
        self.installed.then(|| {
            self.log.made.fetch_add(1, Relaxed);
            Box::new(Fake {
                log: self.log.clone(),
                sounding: 0,
            }) as Box<dyn ExternalDevice>
        })
    }
    fn stem(&mut self, _: &Stem) -> Option<Arc<[f32]>> {
        self.stem.clone()
    }
}

/// A track with the fake plugin and one note in the second beat.
fn project() -> (Project, TrackId, DeviceId) {
    let mut ids = IdGen::new(ActorId(3));
    let (track, clip, device) = (ids.track(), ids.clip(), ids.device());
    let mut content = TrackContent::default();
    content.devices.push_back(Device::new(
        device,
        DeviceKind::Plugin {
            format: PluginFormat::Clap,
            id: "org.example.fake".into(),
            state: None,
        },
    ));
    let mut project = Project::default();
    let note = Note {
        start: Tick::beats(1),
        dur: Tick::beats(1),
        pitch: 64,
        vel: 100,
    };
    apply_all(
        &mut project,
        &[
            Op::TrackAdd {
                id: track,
                shell: TrackShell {
                    name: "t".into(),
                    color: 0,
                },
                content,
                after: None,
            },
            Op::ClipAdd {
                track,
                id: clip,
                clip: Clip {
                    start: Tick::ZERO,
                    length: Tick::bars(1),
                    name: "c".into(),
                    notes: [(ids.note(), note)].into_iter().collect(),
                },
            },
        ],
    );
    (project, track, device)
}

/// Renders `frames` from timeline sample `pos` and returns the left and right of the last
/// frame.
fn last_frame(graph: &mut mdaw_dsp::Graph, pos: i64, frames: usize) -> [f32; 2] {
    let mut out = vec![0.0; frames * 2];
    graph.process(Some(pos), &mut out);
    [out[frames * 2 - 2], out[frames * 2 - 1]]
}

#[test]
fn a_plugin_plays_its_notes_in_stereo_and_is_kept_across_rebuilds() {
    let (project, track, device) = project();
    let log = Arc::new(Log::default());
    let mut source = Source {
        log: log.clone(),
        installed: true,
        stem: None,
    };
    let mut compiler = Compiler::new(RATE);
    let mut graph = compiler.build_with(&project, None, &mut source);
    // Half a second in: the note of the second beat sounds.
    let beat = RATE as i64 / 2;
    assert_eq!(last_frame(&mut graph, 0, 256), [0.0, 0.0]);
    let [left, right] = last_frame(&mut graph, beat, 256);
    // Unity gain in the centre: the channels keep their difference.
    assert!(
        (left - 0.1).abs() < 1e-6 && (right - 0.2).abs() < 1e-6,
        "{left} {right}"
    );
    assert_eq!(log.notes.lock().unwrap().last().map(|n| n.1), Some(64));

    // A change elsewhere in the project: the instance is carried over, not made again.
    let mut changed = project.clone();
    apply(&mut changed, &Op::MasterSetGain { gain: 0.5 });
    let mut next = compiler.build_with(&changed, None, &mut source);
    assert!(!next.adopt_from(&mut graph));
    graph.retire();
    assert_eq!(log.made.load(Relaxed), 1);
    assert_eq!(log.retired.load(Relaxed), 0, "it moved on");

    // Forgotten, as after a new state: made anew, and the old one retires.
    compiler.forget(track, device);
    let mut again = compiler.build_with(&changed, None, &mut source);
    again.adopt_from(&mut next);
    next.retire();
    assert_eq!(log.made.load(Relaxed), 2);
    assert_eq!(log.retired.load(Relaxed), 1);

    // Live notes reach it too.
    again.note_on(track, 7, 70, 90);
    assert!(
        log.notes
            .lock()
            .unwrap()
            .iter()
            .any(|&(key, pitch)| pitch == 70 && key & mdaw_dsp::LIVE_KEY != 0)
    );
}

#[test]
fn without_the_plugin_the_stem_plays_from_the_start_of_the_timeline() {
    let (mut project, track, _) = project();
    // A stem whose every frame holds its own index, so the position can be read off the output.
    let frames = RATE as usize;
    let stem: Arc<[f32]> = (0..frames).flat_map(|i| [i as f32, -(i as f32)]).collect();
    project.set_stem(
        track,
        Some(Stem {
            blob: BlobHash([1; 32]),
            tempo_hash: project.tempo_map().hash(),
            len_samples: frames as u64,
            gain: 1.0,
        }),
    );
    let log = Arc::new(Log::default());
    let mut source = Source {
        log: log.clone(),
        installed: false,
        stem: Some(stem),
    };
    let mut graph = Compiler::new(RATE).build_with(&project, None, &mut source);
    assert_eq!(log.made.load(Relaxed), 0);
    let [left, right] = last_frame(&mut graph, 1000, 100);
    // Frame 1099 of the stem, at the track's unity gain (within rounding of the pan law).
    assert!(
        (left - 1099.0).abs() < 1e-3 && (right + 1099.0).abs() < 1e-3,
        "{left} {right}"
    );
    // Past its end it is silent, and so is a stopped transport.
    assert_eq!(last_frame(&mut graph, frames as i64, 100), [0.0, 0.0]);
    let mut out = vec![0.0; 200];
    graph.process(None, &mut out);
    assert!(out.iter().all(|s| *s == 0.0));

    // Neither plugin nor stem: silent.
    source.stem = None;
    let mut graph = Compiler::new(RATE).build_with(&project, None, &mut source);
    assert_eq!(last_frame(&mut graph, 1000, 100), [0.0, 0.0]);
}
