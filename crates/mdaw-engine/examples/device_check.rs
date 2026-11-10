//! Plays a short quiet phrase on the default output device and reports how the audio thread
//! behaved. `cargo run -p mdaw-engine --example device_check`

use mdaw_dsp::pocket::DSP_VERSION;
use mdaw_engine::Engine;
use mdaw_engine::alloc_guard::{self, GuardedAlloc};
use mdaw_model::*;
use std::time::{Duration, Instant};

#[global_allocator]
static ALLOC: GuardedAlloc = GuardedAlloc;

fn main() {
    let mut ids = IdGen::new(ActorId(1));
    let (track, clip) = (ids.track(), ids.clip());
    let mut ops = vec![
        Op::MasterSetGain { gain: 0.15 },
        Op::TrackAdd {
            id: track,
            shell: TrackShell {
                name: "check".into(),
                color: 0,
            },
            content: TrackContent::default(),
            after: None,
        },
        Op::DeviceAdd {
            track,
            device: Device::new(
                ids.device(),
                DeviceKind::Builtin {
                    kind: BuiltinKind::Pocket,
                    dsp_version: DSP_VERSION,
                },
            ),
            after: None,
        },
        Op::ClipAdd {
            track,
            id: clip,
            clip: Clip {
                start: Tick::ZERO,
                length: Tick::bars(1),
                name: String::new(),
                notes: Default::default(),
            },
        },
    ];
    for (i, pitch) in [57u8, 60, 64, 69].into_iter().enumerate() {
        let note = Note {
            start: Tick(i as i64 * PPQ / 2),
            dur: Tick(PPQ / 2),
            pitch,
            vel: 90,
        };
        ops.push(Op::NoteAdd {
            track,
            clip,
            id: ids.note(),
            note,
        });
    }
    let mut project = Project::default();
    apply_all(&mut project, &ops);

    let mut engine = match Engine::open() {
        Ok(engine) => engine,
        Err(error) => {
            eprintln!("cannot open audio output: {error}");
            std::process::exit(1);
        }
    };
    println!("{}", engine.description());
    engine.set_project(&project);
    engine.set_loop(Some(TickRange::new(Tick::ZERO, Tick::bars(1))));
    engine.play();

    let started = Instant::now();
    let mut peak = 0.0f32;
    let mut edits = 0;
    while started.elapsed() < Duration::from_millis(2500) {
        std::thread::sleep(Duration::from_millis(16));
        // Keep the graph swapping, as a user dragging a knob would.
        edits += 1;
        apply(
            &mut project,
            &Op::MasterSetGain {
                gain: 0.15 + (edits % 10) as f32 * 0.001,
            },
        );
        engine.set_project(&project);
        engine.tick();
        peak = peak.max(engine.take_peak()[0]);
    }
    let position = engine.position();
    engine.stop();
    std::thread::sleep(Duration::from_millis(400));
    engine.tick();

    let stats = engine.stats();
    println!(
        "{} callbacks, {} frames (avg block {}), longest callback {} us, {} overruns, {} stream errors",
        stats.callbacks,
        stats.frames,
        stats.frames / stats.callbacks.max(1),
        stats.max_callback_micros,
        stats.overruns,
        stats.stream_errors
    );
    println!("{edits} graph swaps, peak {peak:.3}, playhead at tick {position:.0} (looping one bar)");
    println!("allocations on the audio thread: {}", alloc_guard::violations());
    let ok = stats.callbacks > 0 && peak > 0.0 && alloc_guard::violations() == 0 && stats.stream_errors == 0;
    std::process::exit(if ok { 0 } else { 1 });
}
