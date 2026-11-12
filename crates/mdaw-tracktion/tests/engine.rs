//! The engine driven from the canonical model, through the safe API, with no audio device.
//!
//! One engine per process, and tests in one binary run on parallel threads: so this is a single
//! `#[test]` whose steps are the helper functions below. Files go to `target/bridge-scratch/`.
//! The CLAP step runs if `target/bridge-scratch/clap/mdaw-test-synth.clap` exists:
//!
//! ```sh
//! cargo build -p mdaw-test-plugin && mkdir -p target/bridge-scratch/clap \
//!     && cp target/debug/libmdaw_test_plugin.so target/bridge-scratch/clap/mdaw-test-synth.clap
//! ```

use mdaw_model::{
    ActorId, BlobHash, BuiltinKind, Clip, ClipId, ClipProp, Device, DeviceId, DeviceKind, IdGen, MixProp,
    Note, NoteId, NoteProp, Op, PluginFormat, Project, TempoId, TempoPoint, Tick, TrackContent, TrackId,
    TrackShell,
};
use mdaw_tracktion::{Config, Engine, Event, Problem, Status, apply_op, load_project};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const Q: i64 = 960;
const RATE: f64 = 44_100.0;
/// Below this a stretch of a render counts as silent, above `SOUND` as sounding.
const SILENT: f64 = 0.001;
const SOUND: f64 = 0.05;
const TEST_SYNTH: &str = "org.mdaw.test-synth";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn scratch() -> PathBuf {
    root().join("target/bridge-scratch")
}

fn config() -> Config {
    Config::new("mdaw-tracktion-test", scratch().join("rust-settings"))
}

// ---- wav ---------------------------------------------------------------------------------------

/// A 24-bit PCM wav file, as floats, interleaved.
struct Wav {
    channels: usize,
    rate: f64,
    samples: Vec<f32>,
}

fn read_wav(path: &Path) -> Wav {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert!(
        data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WAVE",
        "not a wav file"
    );
    let le16 = |p: usize| u16::from_le_bytes([data[p], data[p + 1]]) as usize;
    let le32 = |p: usize| u32::from_le_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]) as usize;
    let (mut channels, mut rate, mut bits, mut pcm) = (0, 0, 0, None);
    let mut pos = 12;
    while pos + 8 <= data.len() {
        let size = le32(pos + 4).min(data.len() - pos - 8);
        let body = pos + 8;
        match &data[pos..pos + 4] {
            b"fmt " if size >= 16 => {
                let format = le16(body);
                assert!(format == 1 || format == 0xFFFE, "wav format {format} is not PCM");
                channels = le16(body + 2);
                rate = le32(body + 4);
                bits = le16(body + 14);
            }
            b"data" => pcm = Some(&data[body..body + size]),
            _ => {}
        }
        pos = body + size + (size & 1);
    }
    assert_eq!(bits, 24, "24-bit expected");
    assert_eq!(channels, 2, "stereo expected");
    let pcm = pcm.expect("a data chunk");
    let samples = pcm
        .chunks_exact(3)
        .map(|s| {
            let v = i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8;
            v as f32 / 8_388_608.0
        })
        .collect();
    Wav {
        channels,
        rate: rate as f64,
        samples,
    }
}

impl Wav {
    fn seconds(&self) -> f64 {
        self.samples.len() as f64 / self.channels as f64 / self.rate
    }

    fn frame_range(&self, from: f64, to: f64) -> std::ops::Range<usize> {
        let frames = self.samples.len() / self.channels;
        let a = ((from * self.rate).max(0.0) as usize).min(frames);
        let b = ((to * self.rate).max(0.0) as usize).min(frames);
        a * self.channels..b.max(a) * self.channels
    }

    fn peak_between(&self, from: f64, to: f64) -> f64 {
        self.samples[self.frame_range(from, to)]
            .iter()
            .fold(0.0f64, |m, s| m.max(s.abs() as f64))
    }

    fn peak(&self) -> f64 {
        self.peak_between(0.0, f64::MAX)
    }

    /// Seconds of the first sample above the threshold.
    fn first_above(&self, threshold: f64) -> Option<f64> {
        let i = self.samples.iter().position(|s| s.abs() as f64 > threshold)?;
        Some((i / self.channels) as f64 / self.rate)
    }

    fn last_above(&self, threshold: f64) -> Option<f64> {
        let i = self.samples.iter().rposition(|s| s.abs() as f64 > threshold)?;
        Some((i / self.channels) as f64 / self.rate)
    }

    /// RMS of each 50 ms window.
    fn envelope(&self) -> Vec<f64> {
        let window = (0.05 * self.rate) as usize * self.channels;
        self.samples
            .chunks(window)
            .map(|w| (w.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / w.len() as f64).sqrt())
            .collect()
    }
}

/// Two renders of the same project agree: equally long, and sounding and silent in the same
/// 50 ms windows (the built-in synth is not sample-repeatable, so samples are not compared).
fn assert_same_envelope(a: &Wav, b: &Wav, step: &str) {
    assert!(
        (a.seconds() - b.seconds()).abs() < 0.01,
        "{step}: lengths {} vs {}",
        a.seconds(),
        b.seconds()
    );
    let (ea, eb) = (a.envelope(), b.envelope());
    let (mut sounding, mut energy_a, mut energy_b) = (0, 0.0, 0.0);
    for (i, (x, y)) in ea.iter().zip(&eb).enumerate() {
        let at = i as f64 * 0.05;
        if *x > 0.01 || *y > 0.01 {
            assert!(
                *x > 0.001 && *y > 0.001,
                "{step}: window at {at:.2}s sounds in one only ({x:.5} vs {y:.5})"
            );
            sounding += 1;
        }
        if *x < 1e-5 || *y < 1e-5 {
            assert!(
                *x < 1e-3 && *y < 1e-3,
                "{step}: window at {at:.2}s silent in one only ({x:.5} vs {y:.5})"
            );
        }
        energy_a += x * x;
        energy_b += y * y;
    }
    assert!(sounding > 0, "{step}: nothing sounds");
    let ratio = (energy_a / energy_b).sqrt();
    assert!(
        (0.7..1.4).contains(&ratio),
        "{step}: overall level differs by {ratio:.3}"
    );
}

fn render(engine: &Engine, name: &str, end_tick: i64) -> Wav {
    let path = scratch().join(format!("rust-{name}.wav"));
    let _ = std::fs::remove_file(&path);
    engine
        .render(&path, 0, end_tick, RATE)
        .unwrap_or_else(|e| panic!("{name}: render failed: {e}"));
    let wav = read_wav(&path);
    println!(
        "{name}: {:.3}s peak {:.4} first {:?} last {:?}",
        wav.seconds(),
        wav.peak(),
        wav.first_above(SILENT),
        wav.last_above(SILENT)
    );
    wav
}

fn near(value: Option<f64>, expected: f64, tolerance: f64) -> bool {
    value.is_some_and(|v| (v - expected).abs() < tolerance)
}

// ---- the model side ------------------------------------------------------------------------------

/// The canonical project and the blobs it refers to, kept in step with the engine.
struct World {
    engine: Engine,
    model: Project,
    blobs: HashMap<BlobHash, Vec<u8>>,
}

impl World {
    /// Applies an op to the model, then to the engine.
    fn op(&mut self, op: Op) -> Vec<Problem> {
        let before = self.model.clone();
        mdaw_model::apply(&mut self.model, &op);
        self.engine_op(&before, &op)
    }

    /// Applies an op to the engine only (to repeat one), with `before` as the state before it.
    fn engine_op(&self, before: &Project, op: &Op) -> Vec<Problem> {
        let blobs = |h: &BlobHash| self.blobs.get(h).cloned();
        apply_op(&self.engine, before, op, &self.model, &blobs).unwrap_or_else(|e| panic!("{op:?}: {e}"))
    }

    fn load(&self) -> Vec<Problem> {
        let blobs = |h: &BlobHash| self.blobs.get(h).cloned();
        load_project(&self.engine, &self.model, &blobs).expect("load_project")
    }
}

struct Ids {
    tempo: TempoId,
    tempo2: TempoId,
    track: TrackId,
    synth: DeviceId,
    clip: ClipId,
    /// Beats 0, 1, 2 and 3 of the clip.
    first: [NoteId; 4],
    /// Beat 6 of the clip, added later.
    late: NoteId,
}

fn note(beat: i64, pitch: u8) -> Note {
    Note {
        start: Tick(beat * Q),
        dur: Tick(Q / 2),
        pitch,
        vel: 100,
    }
}

/// The starting project, built with the model's own ops: 120 bpm, one track with the built-in
/// synth, a clip of 8 beats with notes on beats 0 to 3.
fn build_model(ids_gen: &mut IdGen) -> (Project, Vec<Op>, Ids) {
    let ids = Ids {
        tempo: ids_gen.tempo(),
        tempo2: ids_gen.tempo(),
        track: ids_gen.track(),
        synth: ids_gen.device(),
        clip: ids_gen.clip(),
        first: [ids_gen.note(), ids_gen.note(), ids_gen.note(), ids_gen.note()],
        late: ids_gen.note(),
    };
    let mut synth = Device::new(
        ids.synth,
        DeviceKind::Builtin {
            kind: BuiltinKind::Pocket,
            dsp_version: 1,
        },
    );
    synth.params.insert(0, 0.5);
    let mut ops = vec![
        Op::TempoPut {
            id: ids.tempo,
            point: TempoPoint {
                tick: Tick(0),
                milli_bpm: 120_000,
            },
        },
        Op::TrackAdd {
            id: ids.track,
            shell: TrackShell {
                name: "Synth".into(),
                color: 0x33_66_99,
            },
            content: TrackContent::default(),
            after: None,
        },
        Op::DeviceAdd {
            track: ids.track,
            device: synth,
            after: None,
        },
        Op::ClipAdd {
            track: ids.track,
            id: ids.clip,
            clip: Clip {
                start: Tick(0),
                length: Tick(8 * Q),
                name: "Clip".into(),
                notes: Default::default(),
            },
        },
    ];
    for (i, id) in ids.first.iter().enumerate() {
        ops.push(Op::NoteAdd {
            track: ids.track,
            clip: ids.clip,
            id: *id,
            note: note(i as i64, [60, 64, 67, 72][i]),
        });
    }
    let mut project = Project::default();
    for op in &ops {
        assert!(
            mdaw_model::apply(&mut project, op).is_some(),
            "model op did nothing: {op:?}"
        );
    }
    (project, ops, ids)
}

// ---- the steps -------------------------------------------------------------------------------------

#[test]
fn the_engine_follows_the_model() {
    let started = Instant::now();
    std::fs::create_dir_all(scratch()).unwrap();
    let clap_dir = scratch().join("clap");
    let have_clap = clap_dir.join("mdaw-test-synth.clap").is_file();
    if have_clap {
        // SAFETY: set before the engine (and any other thread of this test binary that could
        // read the environment) starts; this binary has a single test.
        unsafe { std::env::set_var("CLAP_PATH", &clap_dir) };
    }

    let engine = Engine::new(config()).expect("create the engine");
    println!("versions: {}", engine.versions());
    assert!(engine.versions().contains("JUCE"), "versions");
    assert_eq!(mdaw_tracktion::abi_version(), 1);
    one_engine_per_process();

    let mut ids_gen = IdGen::new(ActorId(7));
    let (model, _, ids) = build_model(&mut ids_gen);
    let mut world = World {
        engine,
        model,
        blobs: HashMap::new(),
    };

    loaded_project_renders(&world);
    incremental_ops(&mut world, &ids);
    other_ops(&mut world, &ids, &mut ids_gen);
    incremental_equals_reload(&mut world, &ids, &mut ids_gen);
    repeated_ops_are_idempotent(&mut world, &ids, &mut ids_gen);
    params_and_state(&world, &ids);
    no_events_for_own_changes(&world);
    track_removed(&mut world, &ids);
    plugins(&mut world, &mut ids_gen, have_clap);

    drop(world);
    second_life();
    println!("whole test: {:?}", started.elapsed());
}

/// A second engine is refused while one lives.
fn one_engine_per_process() {
    let second = Engine::new(config());
    let error = second.expect_err("one engine per process: second create refused");
    println!("second create: {error}");
    assert!(
        !error.message.is_empty(),
        "one engine per process: the refusal says why"
    );
}

/// Step 1: the model project, loaded and rendered: sound where the notes are, silence after.
fn loaded_project_renders(world: &World) {
    let problems = world.load();
    println!("load problems: {problems:?}");
    assert!(
        problems
            .iter()
            .any(|p| matches!(p, Problem::ParamNotMapped { param: 0, .. })),
        "load: the Pocket parameter is reported as not carried over"
    );
    assert_eq!(problems.len(), 1, "load: no other problems");

    let wav = render(&world.engine, "loaded", 16 * Q);
    assert!(
        (wav.seconds() - 8.0).abs() < 0.05,
        "load: 16 beats at 120 bpm are 8 s"
    );
    assert!(
        wav.peak_between(0.0, 1.9) > SOUND,
        "load: notes on beats 0-3 sound"
    );
    assert!(
        near(wav.first_above(SILENT), 0.0, 0.03),
        "load: sound starts on beat 0"
    );
    assert!(
        wav.peak_between(2.6, 8.0) < SILENT,
        "load: silent after the notes"
    );
}

/// Step 2: ops one at a time, to the model and the engine, with a render after each.
fn incremental_ops(world: &mut World, ids: &Ids) {
    // A note on beat 6 (3 s), where it was silent.
    world.op(Op::NoteAdd {
        track: ids.track,
        clip: ids.clip,
        id: ids.late,
        note: note(6, 76),
    });
    let wav = render(&world.engine, "note_added", 16 * Q);
    assert!(wav.peak_between(2.98, 3.4) > SOUND, "note added: beat 6 sounds");
    assert!(
        wav.peak_between(2.6, 2.98) < SILENT,
        "note added: still silent before it"
    );
    assert!(wav.peak_between(4.2, 8.0) < SILENT, "note added: silent after it");

    // The first four notes removed.
    for id in ids.first {
        world.op(Op::NoteRemove {
            track: ids.track,
            clip: ids.clip,
            id,
        });
    }
    let wav = render(&world.engine, "notes_removed", 16 * Q);
    assert!(
        wav.peak_between(0.0, 2.98) < SILENT,
        "notes removed: beats 0-3 silent"
    );
    assert!(
        near(wav.first_above(SILENT), 3.0, 0.03),
        "notes removed: beat 6 still sounds"
    );

    // The clip moved to beat 4: its note on beat 6 is now on beat 10 (5 s).
    world.op(Op::ClipSet {
        track: ids.track,
        id: ids.clip,
        prop: ClipProp::Start(Tick(4 * Q)),
    });
    let wav = render(&world.engine, "clip_moved", 16 * Q);
    assert!(
        near(wav.first_above(SILENT), 5.0, 0.03),
        "clip moved: the note moved with it"
    );
    assert!(wav.peak_between(5.0, 5.4) > SOUND, "clip moved: it sounds");

    // Mute, unmute.
    world.op(Op::TrackSetMix {
        track: ids.track,
        prop: MixProp::Mute(true),
    });
    let wav = render(&world.engine, "muted", 16 * Q);
    assert!(wav.peak() < SILENT, "muted: silent");
    world.op(Op::TrackSetMix {
        track: ids.track,
        prop: MixProp::Mute(false),
    });
    let wav = render(&world.engine, "unmuted", 16 * Q);
    assert!(near(wav.first_above(SILENT), 5.0, 0.03), "unmuted: sounds again");

    // 240 bpm: beat 10 is at 2.5 s, and 16 beats last 4 s.
    world.op(Op::TempoPut {
        id: ids.tempo,
        point: TempoPoint {
            tick: Tick(0),
            milli_bpm: 240_000,
        },
    });
    let wav = render(&world.engine, "tempo240", 16 * Q);
    assert!((wav.seconds() - 4.0).abs() < 0.05, "tempo 240: 16 beats are 4 s");
    assert!(
        near(wav.first_above(SILENT), 2.5, 0.03),
        "tempo 240: the note is at 2.5 s"
    );

    // A second point, 60 bpm from beat 8: beat 10 is at 2 s + 2 s.
    world.op(Op::TempoPut {
        id: ids.tempo2,
        point: TempoPoint {
            tick: Tick(8 * Q),
            milli_bpm: 60_000,
        },
    });
    let wav = render(&world.engine, "tempo_two_points", 16 * Q);
    assert!(
        near(wav.first_above(SILENT), 4.0, 0.03),
        "two tempo points: the note is at 4 s"
    );
    world.op(Op::TempoRemove { id: ids.tempo2 });
    let wav = render(&world.engine, "tempo_point_removed", 16 * Q);
    assert!(
        near(wav.first_above(SILENT), 2.5, 0.03),
        "tempo point removed: back to 2.5 s"
    );
}

/// Step 2b: the remaining kinds of op, so that the comparison in step 3 covers them too.
fn other_ops(world: &mut World, ids: &Ids, ids_gen: &mut IdGen) {
    // Bypass silences the synth; back on, it sounds.
    let bypass = |bypass| Op::DeviceSetBypass {
        track: ids.track,
        id: ids.synth,
        bypass,
    };
    world.op(bypass(true));
    let wav = render(&world.engine, "bypassed", 16 * Q);
    assert!(wav.peak() < SILENT, "bypass: silent");
    world.op(bypass(false));

    // Master gain a quarter: quieter.
    let reference = render(&world.engine, "before_master", 16 * Q).peak();
    world.op(Op::MasterSetGain { gain: 0.25 });
    let quieter = render(&world.engine, "master_quarter", 16 * Q).peak();
    assert!(
        quieter > SILENT && quieter < 0.6 * reference,
        "master gain: quieter ({quieter} vs {reference})"
    );
    world.op(Op::MasterSetGain { gain: 1.0 });

    // A second device, moved in front and removed again; a second, empty track moved about.
    let extra = ids_gen.device();
    let problems = world.op(Op::DeviceAdd {
        track: ids.track,
        device: Device::new(
            extra,
            DeviceKind::Builtin {
                kind: BuiltinKind::Pocket,
                dsp_version: 1,
            },
        ),
        after: Some(ids.synth),
    });
    assert!(problems.is_empty(), "device add: {problems:?}");
    world.op(Op::DeviceMove {
        track: ids.track,
        id: extra,
        after: None,
    });
    world.op(Op::DeviceRemove {
        track: ids.track,
        id: extra,
    });
    let other = ids_gen.track();
    world.op(Op::TrackAdd {
        id: other,
        shell: TrackShell {
            name: "Empty".into(),
            color: 0,
        },
        content: TrackContent::default(),
        after: Some(ids.track),
    });
    world.op(Op::TrackMove {
        id: other,
        after: None,
    });
    world.op(Op::TrackSetShell {
        id: ids.track,
        prop: mdaw_model::ShellProp::Name("Renamed".into()),
    });
    world.op(Op::TrackSetShell {
        id: ids.track,
        prop: mdaw_model::ShellProp::Color(0xff_00_00),
    });
    world.op(Op::ProjectSetName { name: "Song".into() });
    world.op(Op::TrackSetMix {
        track: ids.track,
        prop: MixProp::Pan(-0.5),
    });
    // The clip shortened to 7 beats keeps its note on clip beat 6; its name is not the engine's.
    world.op(Op::ClipSet {
        track: ids.track,
        id: ids.clip,
        prop: ClipProp::Length(Tick(7 * Q)),
    });
    world.op(Op::ClipSet {
        track: ids.track,
        id: ids.clip,
        prop: ClipProp::Name("Renamed".into()),
    });
    // The late note: up a tone, softer.
    world.op(Op::NoteSet {
        track: ids.track,
        clip: ids.clip,
        id: ids.late,
        prop: NoteProp::Pitch(78),
    });
    world.op(Op::NoteSet {
        track: ids.track,
        clip: ids.clip,
        id: ids.late,
        prop: NoteProp::Vel(90),
    });
    let wav = render(&world.engine, "other_ops", 16 * Q);
    assert!(
        near(wav.first_above(SILENT), 2.5, 0.03),
        "other ops: the note still sounds at 2.5 s"
    );
    // Panned left: the left channel is louder.
    let (left, right) = wav.samples.chunks_exact(2).fold((0.0f32, 0.0f32), |(l, r), f| {
        (l.max(f[0].abs()), r.max(f[1].abs()))
    });
    assert!(left > right * 1.2, "pan: left {left} louder than right {right}");
}

/// Step 3: the engine after the incremental ops sounds like the engine loaded from the final
/// model.
fn incremental_equals_reload(world: &mut World, ids: &Ids, ids_gen: &mut IdGen) {
    // More to compare: notes on clip beats 0 and 2 (song beats 4 and 6: 1 s and 1.5 s).
    for (beat, pitch) in [(0, 62), (2, 65)] {
        world.op(Op::NoteAdd {
            track: ids.track,
            clip: ids.clip,
            id: ids_gen.note(),
            note: note(beat, pitch),
        });
    }
    world.op(Op::NoteSet {
        track: ids.track,
        clip: ids.clip,
        id: ids.late,
        prop: NoteProp::Dur(Tick(Q)),
    });
    world.op(Op::TrackSetMix {
        track: ids.track,
        prop: MixProp::Gain(0.5),
    });
    let incremental = render(&world.engine, "incremental", 16 * Q);

    let problems = world.load();
    let reloaded = render(&world.engine, "reloaded", 16 * Q);
    assert_eq!(
        problems.len(),
        1,
        "reload: only the Pocket parameter is a problem"
    );
    assert_same_envelope(&incremental, &reloaded, "incremental vs reloaded");
}

/// Step 4: the same op twice in the engine has the effect of once.
fn repeated_ops_are_idempotent(world: &mut World, ids: &Ids, ids_gen: &mut IdGen) {
    let reference = render(&world.engine, "before_repeat", 16 * Q);
    let op = Op::NoteSet {
        track: ids.track,
        clip: ids.clip,
        id: ids.late,
        prop: NoteProp::Dur(Tick(Q)),
    };
    let model = world.model.clone();
    world.engine_op(&model, &op);
    world.engine_op(&model, &op);
    let clip_op = Op::ClipSet {
        track: ids.track,
        id: ids.clip,
        prop: ClipProp::Start(Tick(4 * Q)),
    };
    world.engine_op(&model, &clip_op);
    world.engine_op(&model, &clip_op);
    let repeated = render(&world.engine, "repeated", 16 * Q);
    assert_same_envelope(&reference, &repeated, "repeated ops");

    // A note added twice is there once: removing it once leaves silence where it was.
    // Clip beat 4 is song beat 8, 2 s at 240 bpm; the nearest notes end by 1.75 s and start at 2.5 s.
    let id = ids_gen.note();
    let add = Op::NoteAdd {
        track: ids.track,
        clip: ids.clip,
        id,
        note: Note {
            start: Tick(4 * Q),
            dur: Tick(Q / 4),
            pitch: 70,
            vel: 100,
        },
    };
    let before = world.model.clone();
    world.op(add.clone());
    world.engine_op(&before, &add);
    let wav = render(&world.engine, "added_twice", 16 * Q);
    assert!(wav.peak_between(2.0, 2.3) > SOUND, "added twice: the note sounds");
    world.op(Op::NoteRemove {
        track: ids.track,
        clip: ids.clip,
        id,
    });
    let wav = render(&world.engine, "removed_once", 16 * Q);
    assert!(
        wav.peak_between(2.0, 2.45) < SILENT,
        "added twice, removed once: gone"
    );
}

/// Step 5: parameters and state of the built-in synth, through the safe API.
fn params_and_state(world: &World, ids: &Ids) {
    let engine = &world.engine;
    let device = ids.synth.0.0;
    let params = engine.device_params(device).expect("params");
    assert!(!params.is_empty(), "params: listed");
    let param = params
        .iter()
        .find(|p| p.steps == 0)
        .expect("params: a continuous one");
    println!("params: {} of them; using {:?}", params.len(), param);
    assert_eq!(
        engine.device_params(999).expect("params of nothing"),
        vec![],
        "params: none for a missing device"
    );

    engine
        .device_set_param(device, &param.id, 0.25)
        .expect("set param");
    let value = engine.device_get_param(device, &param.id).expect("get param");
    assert!((value - 0.25).abs() < 0.01, "params: read back {value}");
    let unknown = engine
        .device_set_param(device, "no-such-param", 0.5)
        .expect_err("unknown param");
    assert_eq!(unknown.status, Status::NotFound, "params: unknown id is NotFound");
    assert!(!unknown.message.is_empty(), "params: the error has a message");
    let nul = engine
        .device_set_param(device, "a\0b", 0.5)
        .expect_err("interior NUL");
    assert_eq!(nul.status, Status::Argument, "params: interior NUL refused");

    let state = engine.device_get_state(device);
    assert!(!state.is_empty(), "state: read");
    assert!(
        engine.device_get_state(999).is_empty(),
        "state: none for a missing device"
    );
    engine
        .device_set_param(device, &param.id, 0.75)
        .expect("set param");
    let value = engine.device_get_param(device, &param.id).expect("get param");
    assert!((value - 0.75).abs() < 0.01, "state: param changed to {value}");
    engine.device_set_state(device, &state).expect("set state");
    let value = engine.device_get_param(device, &param.id).expect("get param");
    assert!(
        (value - 0.25).abs() < 0.01,
        "state: restoring it restores the param ({value})"
    );
}

/// Step 6: the engine's own changes are not echoed as events.
fn no_events_for_own_changes(world: &World) {
    std::thread::sleep(Duration::from_millis(300));
    let mut events = Vec::new();
    while let Some(event) = world.engine.poll_event() {
        events.push(event);
    }
    println!("events: {events:?}");
    // Device-list changes are reported even without a device (Tracktion opens MIDI ports).
    assert!(
        events.iter().all(|e| *e == Event::DeviceChanged),
        "events: nothing but device changes after own changes: {events:?}"
    );
    assert_eq!(world.engine.poll_event(), None, "events: drained");
    let state = world.engine.state();
    assert!(
        !state.device_open && !state.playing,
        "state: no device, not playing: {state:?}"
    );
}

/// Step 7: removing the track leaves silence.
fn track_removed(world: &mut World, ids: &Ids) {
    world.op(Op::TrackRemove { id: ids.track });
    assert!(
        world.model.track(ids.track).is_none(),
        "track removed: gone from the model"
    );
    let wav = render(&world.engine, "track_removed", 16 * Q);
    assert!(wav.peak() < SILENT, "track removed: silent");
}

/// Step 8: plugin devices. One whose plugin is not installed is left out and reported; the
/// CLAP test synth, if built, plays, and its state comes back through the blob resolver.
fn plugins(world: &mut World, ids_gen: &mut IdGen, have_clap: bool) {
    if have_clap {
        let engine = &world.engine;
        engine.plugins_scan("CLAP").expect("scan CLAP");
        let list = engine.plugins().expect("plugin list");
        println!("plugins: {list:?}");
    } else {
        println!(
            "NOTE: target/bridge-scratch/clap/mdaw-test-synth.clap is missing; the CLAP synth is not played"
        );
    }

    let (track, synth, missing, clip) = (
        ids_gen.track(),
        ids_gen.device(),
        ids_gen.device(),
        ids_gen.clip(),
    );
    let plugin = |device, id: &str| {
        Device::new(
            device,
            DeviceKind::Plugin {
                format: PluginFormat::Clap,
                id: id.into(),
                state: None,
            },
        )
    };
    let mut content = TrackContent::default();
    content
        .devices
        .push_back(plugin(missing, "org.example.not-installed"));
    content.devices.push_back(plugin(synth, TEST_SYNTH));
    let mut notes = mdaw_model::Clip {
        start: Tick(0),
        length: Tick(4 * Q),
        name: "Ext".into(),
        notes: Default::default(),
    };
    for beat in 0..4 {
        notes.notes.insert(ids_gen.note(), note(beat, 60 + beat as u8));
    }
    content.clips.insert(clip, notes.into());
    world.op(Op::TrackAdd {
        id: track,
        shell: TrackShell {
            name: "External".into(),
            color: 0,
        },
        content,
        after: None,
    });
    let problems = world.load();
    println!("plugin load problems: {problems:?}");
    let skipped = |device: DeviceId| {
        problems
            .iter()
            .any(|p| matches!(p, Problem::DeviceSkipped { device: d, .. } if *d == device))
    };
    assert!(skipped(missing), "plugins: the missing one is reported");
    assert_eq!(
        skipped(synth),
        !have_clap,
        "plugins: the test synth is skipped only when not built"
    );
    if !have_clap {
        return;
    }

    // Model tempo is still 240 bpm: 4 beats are 1 s.
    let engine = &world.engine;
    let wav = render(engine, "clap", 8 * Q);
    assert!(wav.peak_between(0.0, 1.0) > 0.01, "clap: the test synth sounds");
    let raw = synth.0.0;
    let params = engine.device_params(raw).expect("clap params");
    println!("clap params: {params:?}");
    // The test synth's parameter 0 is its level, 0..1, so normalised and plain agree.
    engine.device_set_param(raw, "0", 0.2).expect("clap set level");
    render(engine, "clap_level", 8 * Q); // the value reaches the plugin with a processed block
    let state = engine.device_get_state(raw);
    assert!(!state.is_empty(), "clap: state read");
    let blob = BlobHash([0x5a; 32]);
    world.blobs.insert(blob, state);

    world
        .engine
        .device_set_param(raw, "0", 0.9)
        .expect("clap set level");
    render(&world.engine, "clap_level_changed", 8 * Q);
    let problems = world.op(Op::DeviceSetState {
        track,
        id: synth,
        state: Some(blob),
    });
    assert!(problems.is_empty(), "clap: state op applied: {problems:?}");
    let level = world.engine.device_get_param(raw, "0").expect("clap get level");
    assert!(
        (level - 0.2).abs() < 0.01,
        "clap: state restored through the resolver ({level})"
    );

    // Back to no state: the plugin as it starts (level 0.5), made again in place.
    world.op(Op::DeviceSetState {
        track,
        id: synth,
        state: None,
    });
    let level = world.engine.device_get_param(raw, "0").expect("clap get level");
    assert!(
        (level - 0.5).abs() < 0.01,
        "clap: no state is the default level ({level})"
    );
    let wav = render(&world.engine, "clap_recreated", 8 * Q);
    assert!(
        wav.peak_between(0.0, 1.0) > 0.01,
        "clap: the made-again synth sounds"
    );
}

/// After the engine is gone a new one can be made, and it works.
fn second_life() {
    let engine = Engine::new(config()).expect("second life: create after drop");
    let mut ids_gen = IdGen::new(ActorId(9));
    let (model, _, _) = build_model(&mut ids_gen);
    load_project(&engine, &model, &|_| None).expect("second life: load");
    let wav = render(&engine, "second_life", 8 * Q);
    assert!(wav.peak() > SOUND, "second life: renders");
}
