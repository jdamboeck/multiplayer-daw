//! Measurement of the arrangement's repaint with many notes in its clips: 16 tracks, each with
//! one clip of 64 bars holding 2000 notes (32 000 in all), against the same project without
//! notes. For each it times `harness.step()` and the tessellation of the frame's shapes.
//!
//! Not a check: it prints one `RESULT` line per project, with the median and the 95th percentile
//! of both times in milliseconds and the number of shapes of the last frame. Run by hand:
//!
//! ```sh
//! cargo test --release -p mdaw-client --test arrange_bench -- --ignored --nocapture
//! ```

use egui::vec2;
use egui_kittest::Harness;
use mdaw_client::{DawApp, Session, SilentAudio, builtin_kind};
use mdaw_model::{
    ActorId, BuiltinKind, Clip, Device, IdGen, Note, Op, Project, TICKS_PER_BAR, TempoPoint, Tick,
    TrackContent, TrackShell, apply_all,
};
use std::time::{Duration, Instant};

const TRACKS: usize = 16;
const NOTES_PER_CLIP: i64 = 2000;
const CLIP_BARS: i64 = 64;
const WARM_UP: usize = 20;
const SAMPLES: usize = 200;

fn project(with_notes: bool) -> Project {
    let mut ids = IdGen::new(ActorId(7));
    let mut project = Project::default();
    let mut ops = vec![Op::TempoPut {
        id: ids.tempo(),
        point: TempoPoint {
            tick: Tick::ZERO,
            milli_bpm: mdaw_model::DEFAULT_MILLI_BPM,
        },
    }];
    let length = CLIP_BARS * TICKS_PER_BAR;
    let mut after = None;
    for index in 0..TRACKS {
        let track = ids.track();
        let mut content = TrackContent::default();
        content
            .devices
            .push_back(Device::new(ids.device(), builtin_kind(BuiltinKind::TwoOsc)));
        ops.push(Op::TrackAdd {
            id: track,
            shell: TrackShell {
                name: format!("Track {}", index + 1),
                color: 0,
            },
            content,
            after,
        });
        after = Some(track);
        let mut clip = Clip {
            start: Tick::ZERO,
            length: Tick(length),
            name: format!("Clip {}", index + 1),
            notes: Default::default(),
        };
        if with_notes {
            for i in 0..NOTES_PER_CLIP {
                let note = Note {
                    start: Tick(i * length / NOTES_PER_CLIP),
                    dur: Tick(length / NOTES_PER_CLIP),
                    pitch: (36 + i * 49 / NOTES_PER_CLIP) as u8,
                    vel: 100,
                };
                clip.notes.insert(ids.note(), note);
            }
        }
        ops.push(Op::ClipAdd {
            track,
            id: ids.clip(),
            clip,
        });
    }
    apply_all(&mut project, &ops);
    project
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn median_p95(mut times: Vec<f64>) -> (f64, f64) {
    times.sort_by(f64::total_cmp);
    let median = times[times.len() / 2];
    let p95 = times[(times.len() * 95 / 100).min(times.len() - 1)];
    (median, p95)
}

fn measure(name: &str, project: Project) {
    let app = DawApp::new(Session::local(project, SilentAudio::manual()));
    let mut harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .with_step_dt(1.0 / 60.0)
        .with_context_setup(DawApp::setup)
        .build_ui_state(|ui, app: &mut DawApp| app.ui(ui), app);
    harness.run_steps(WARM_UP);
    let mut steps = Vec::with_capacity(SAMPLES);
    let mut tessellations = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        harness.step();
        steps.push(ms(started.elapsed()));
        let started = Instant::now();
        let primitives = harness
            .ctx
            .tessellate(harness.output().shapes.clone(), harness.output().pixels_per_point);
        tessellations.push(ms(started.elapsed()));
        std::hint::black_box(primitives);
    }
    let shapes = harness.output().shapes.len();
    let (step_median, step_p95) = median_p95(steps);
    let (tess_median, tess_p95) = median_p95(tessellations);
    println!(
        "RESULT {name} step_median {step_median:.3} ms step_p95 {step_p95:.3} ms \
         tessellate_median {tess_median:.3} ms tessellate_p95 {tess_p95:.3} ms shapes {shapes}"
    );
}

#[test]
#[ignore = "a measurement, run by hand"]
fn arrange_repaint() {
    measure("notes", project(true));
    measure("empty", project(false));
}
