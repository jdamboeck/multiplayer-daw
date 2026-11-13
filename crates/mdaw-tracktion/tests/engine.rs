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
use mdaw_tracktion::{
    Config, Engine, EntityKind, Event, Problem, Status, apply_op, load_project, sync_project,
};
use mdaw_tracktion_sys as sys;
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

    /// Peaks of the left and the right channel.
    fn channel_peaks(&self) -> (f64, f64) {
        self.samples.chunks_exact(2).fold((0.0, 0.0), |(l, r), f| {
            (f64::max(l, f[0].abs() as f64), f64::max(r, f[1].abs() as f64))
        })
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
    assert_eq!(mdaw_tracktion::abi_version(), 4);
    assert!(
        !engine.versions().contains("Tracktion Engine Tracktion Engine"),
        "versions: name not doubled"
    );
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
    pocket_params(&mut world, &ids);
    no_events_for_own_changes(&world);
    track_removed(&mut world, &ids);
    sync_by_difference(&world, &mut ids_gen);
    world.load();
    plugins(&mut world, &mut ids_gen, have_clap);

    drop(world);
    second_life();
    println!("whole test: {:?}", started.elapsed());
}

/// A second engine is refused while one lives.
fn one_engine_per_process() {
    let second = Engine::new(config());
    let error = second.expect_err("one engine per process: second create refused");
    // Read on this thread right after the failed create, as the Rust side did.
    let raw = sys::mdaw_last_status();
    println!("second create: {error} (C status {raw})");
    assert_eq!(raw, sys::MDAW_ERR_INTERNAL, "second create: the C side's status");
    assert_eq!(
        error.status,
        Status::Internal,
        "second create: the error carries the C side's status"
    );
    assert!(
        !error.message.is_empty(),
        "one engine per process: the refusal says why"
    );
}

/// The raw ids of the first track, its device, its clip and the tempo point at 0.
fn world_ids(world: &World) -> (u64, u64, u64, u64) {
    let (track_id, track) = world.model.ordered_tracks().next().expect("a track");
    let content = track.content.as_ref();
    let device = content.devices.front().expect("a device").id;
    let clip = *content.clips.keys().next().expect("a clip");
    let tempo = *world.model.tempo.keys().next().expect("a tempo point");
    (track_id.0.0, device.0.0, clip.0.0, tempo.0.0)
}

/// Step 1: the model project, loaded and rendered: sound where the notes are, silence after.
fn loaded_project_renders(world: &World) {
    let problems = world.load();
    println!("load problems: {problems:?}");
    // The Pocket parameter (Osc 1 wave) is carried over to the engine's synth.
    assert!(problems.is_empty(), "load: no problems: {problems:?}");
    let engine = &world.engine;
    let ids = &world_ids(world);
    assert!(
        engine.has(EntityKind::Track, ids.0)
            && engine.has(EntityKind::Device, ids.1)
            && engine.has(EntityKind::Clip, ids.2)
            && engine.has(EntityKind::Tempo, ids.3),
        "load: has each kind"
    );
    assert!(
        !engine.has(EntityKind::Track, ids.1) && !engine.has(EntityKind::Device, ids.0),
        "load: has nothing of the wrong kind"
    );

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
    assert!(
        !world.engine.has(EntityKind::Tempo, ids.tempo2.0.0),
        "has: no second tempo point yet"
    );
    world.op(Op::TempoPut {
        id: ids.tempo2,
        point: TempoPoint {
            tick: Tick(8 * Q),
            milli_bpm: 60_000,
        },
    });
    assert!(
        world.engine.has(EntityKind::Tempo, ids.tempo2.0.0),
        "has: the second tempo point"
    );
    let wav = render(&world.engine, "tempo_two_points", 16 * Q);
    assert!(
        near(wav.first_above(SILENT), 4.0, 0.03),
        "two tempo points: the note is at 4 s"
    );
    world.op(Op::TempoRemove { id: ids.tempo2 });
    assert!(
        !world.engine.has(EntityKind::Tempo, ids.tempo2.0.0),
        "has: second tempo point removed"
    );
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
    assert!(
        !world.engine.has(EntityKind::Device, extra.0.0),
        "has: no extra device yet"
    );
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
    assert!(
        world.engine.has(EntityKind::Device, extra.0.0),
        "has: the extra device"
    );
    world.op(Op::DeviceMove {
        track: ids.track,
        id: extra,
        after: None,
    });
    world.op(Op::DeviceRemove {
        track: ids.track,
        id: extra,
    });
    assert!(
        !world.engine.has(EntityKind::Device, extra.0.0),
        "has: extra device removed"
    );
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
    let problems = world.op(Op::ClipSet {
        track: ids.track,
        id: ids.clip,
        prop: ClipProp::Name("Renamed".into()),
    });
    assert!(problems.is_empty(), "clip rename: {problems:?}");
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
    assert!(problems.is_empty(), "reload: no problems: {problems:?}");
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

/// A crude measure of high-frequency content: the RMS of the sample-to-sample difference of
/// the left channel over its RMS. A low-passed signal changes less from sample to sample.
fn brightness(wav: &Wav) -> f64 {
    let left: Vec<f64> = wav.samples.chunks_exact(2).map(|f| f64::from(f[0])).collect();
    let rms = (left.iter().map(|s| s * s).sum::<f64>() / left.len() as f64).sqrt();
    let diff = (left.windows(2).map(|w| (w[1] - w[0]).powi(2)).sum::<f64>() / left.len() as f64).sqrt();
    diff / rms.max(1e-12)
}

/// Step 5b: Pocket's parameters reach the engine's synth (Tracktion's 4osc, an approximation of
/// Pocket, apply.rs). The texts 4osc shows for the converted values confirm the conversions;
/// release, volume and cutoff are heard in renders.
fn pocket_params(world: &mut World, ids: &Ids) {
    // A Pocket parameter set through the model, then back to its default.
    let set = |world: &mut World, param: u32, value: Option<f64>| {
        let problems = world.op(Op::DeviceSetParam {
            track: ids.track,
            id: ids.synth,
            param,
            value,
        });
        assert!(problems.is_empty(), "pocket param {param}: {problems:?}");
    };
    // The state-applying step before may have changed 4osc parameters directly: start from the
    // model's values again.
    set(world, 19, None);
    let device = ids.synth.0.0;
    let text = |world: &World, id: &str| {
        let params = world.engine.device_params(device).expect("4osc params");
        params
            .iter()
            .find(|p| p.id == id)
            .unwrap_or_else(|| panic!("4osc has no {id}: {params:?}"))
            .text
            .clone()
    };
    // Pocket's defaults as 4osc shows them.
    for (id, expected) in [
        ("ampRelease", "300ms"),
        ("ampAttack", "5ms"),
        ("ampSustain", "70.0%"),
        ("filterFreq", "2500Hz"),
        ("masterLevel", "-3.10"),
        ("level1", "-6.02dB"),
        ("tune2", "0st"),
    ] {
        assert_eq!(text(world, id), expected, "pocket default on 4osc: {id}");
    }

    let reference = render(&world.engine, "pocket_reference", 16 * Q);
    let reference_end = reference.last_above(SILENT).expect("pocket: sounds");

    // Release 2 s instead of 0.3 s: the last note rings on for longer.
    set(world, 10, Some(2.0));
    assert_eq!(text(world, "ampRelease"), "2.00s", "pocket release on 4osc");
    let long = render(&world.engine, "pocket_long_release", 16 * Q);
    let long_end = long.last_above(SILENT).expect("pocket long release: sounds");
    println!("pocket release: sound ends at {reference_end:.3} s, with 2 s release at {long_end:.3} s");
    assert!(
        long_end > reference_end + 1.0,
        "pocket release: longer tail ({reference_end} -> {long_end})"
    );
    set(world, 10, None);

    // Volume 0.2 instead of 0.7: about 11 dB quieter.
    set(world, 19, Some(0.2));
    let quiet = render(&world.engine, "pocket_quiet", 16 * Q);
    println!(
        "pocket volume: peak {:.4} -> {:.4}",
        reference.peak(),
        quiet.peak()
    );
    assert!(
        quiet.peak() > SILENT && quiet.peak() < 0.5 * reference.peak(),
        "pocket volume: quieter ({} vs {})",
        quiet.peak(),
        reference.peak()
    );
    set(world, 19, None);

    // Cutoff 150 Hz (and no filter envelope) against 12 kHz: much less high-frequency content.
    set(world, 13, Some(0.0));
    set(world, 11, Some(12_000.0));
    let bright = brightness(&render(&world.engine, "pocket_bright", 16 * Q));
    set(world, 11, Some(150.0));
    assert_eq!(text(world, "filterFreq"), "150Hz", "pocket cutoff on 4osc");
    let dark = brightness(&render(&world.engine, "pocket_dark", 16 * Q));
    println!("pocket cutoff: brightness {bright:.4} at 12 kHz, {dark:.4} at 150 Hz");
    assert!(dark < 0.5 * bright, "pocket cutoff: darker ({bright} -> {dark})");
    set(world, 11, None);
    set(world, 13, None);

    // Back at the defaults: as at the start.
    let again = render(&world.engine, "pocket_defaults_again", 16 * Q);
    assert_same_envelope(&reference, &again, "pocket defaults again");

    // A parameter Pocket does not have is still reported.
    let problems = world.op(Op::DeviceSetParam {
        track: ids.track,
        id: ids.synth,
        param: 99,
        value: Some(1.0),
    });
    assert!(
        matches!(problems.as_slice(), [Problem::ParamNotMapped { param: 99, .. }]),
        "pocket: unknown parameter reported: {problems:?}"
    );
    world.op(Op::DeviceSetParam {
        track: ids.track,
        id: ids.synth,
        param: 99,
        value: None,
    });
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
    let engine = &world.engine;
    assert!(
        !engine.has(EntityKind::Track, ids.track.0.0)
            && !engine.has(EntityKind::Clip, ids.clip.0.0)
            && !engine.has(EntityKind::Device, ids.synth.0.0),
        "has: the track and what was on it are gone"
    );
}

/// Step 7b: `sync_project` brings the engine from one state of the project to the next by their
/// difference. Edit sequences with additions, removals, moves, an undo of everything back to an
/// earlier state, and a rollback (a jump back to an earlier state), each synced step by step,
/// end in an engine that sounds like a fresh load of the final state. A single changed note
/// costs one call.
fn sync_by_difference(world: &World, ids_gen: &mut IdGen) {
    let engine = &world.engine;
    let no_blobs = |_: &BlobHash| None;
    // One oscillator only (Pocket's oscillator mix at 0): two free-running oscillators sum to a
    // level that changes by some 3 dB from render to render, one does not.
    let pocket = |id| {
        let mut device = Device::new(
            id,
            DeviceKind::Builtin {
                kind: BuiltinKind::Pocket,
                dsp_version: 1,
            },
        );
        device.params.insert(4, 0.0);
        device
    };
    let (t1, t2, t3) = (ids_gen.track(), ids_gen.track(), ids_gen.track());
    let (d1, d2, d3) = (ids_gen.device(), ids_gen.device(), ids_gen.device());
    let (c1, c2, c3) = (ids_gen.clip(), ids_gen.clip(), ids_gen.clip());
    let (tempo0, tempo1) = (ids_gen.tempo(), ids_gen.tempo());
    let n: Vec<NoteId> = (0..8).map(|_| ids_gen.note()).collect();
    let shell = |name: &str| TrackShell {
        name: name.into(),
        color: 0,
    };
    let clip = |start: i64, beats: i64| Clip {
        start: Tick(start * Q),
        length: Tick(beats * Q),
        name: "c".into(),
        notes: Default::default(),
    };

    // The starting state: 120 bpm, one track with a clip of two notes.
    let mut model = Project::default();
    let apply = |model: &mut Project, ops: Vec<Op>| -> Vec<Op> {
        let mut inverses = Vec::new();
        for op in &ops {
            if let Some(inverse) = mdaw_model::apply(model, op) {
                inverses.push(inverse);
            }
        }
        inverses
    };
    apply(
        &mut model,
        vec![
            Op::TempoPut {
                id: tempo0,
                point: TempoPoint {
                    tick: Tick(0),
                    milli_bpm: 120_000,
                },
            },
            Op::TrackAdd {
                id: t1,
                shell: shell("one"),
                content: TrackContent::default(),
                after: None,
            },
            Op::DeviceAdd {
                track: t1,
                device: pocket(d1),
                after: None,
            },
            Op::ClipAdd {
                track: t1,
                id: c1,
                clip: clip(0, 8),
            },
            Op::NoteAdd {
                track: t1,
                clip: c1,
                id: n[0],
                note: note(0, 60),
            },
            Op::NoteAdd {
                track: t1,
                clip: c1,
                id: n[1],
                note: note(2, 64),
            },
        ],
    );
    load_project(engine, &model, &no_blobs).expect("sync: first load");
    let start = model.clone();

    // Syncs each step from the one before and checks the end against a fresh load.
    let mut synced = model.clone();
    let step = |synced: &mut Project, next: &Project, what: &str| {
        let problems =
            sync_project(engine, synced, next, &no_blobs).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert!(problems.is_empty(), "{what}: {problems:?}");
        *synced = next.clone();
    };
    let compare = |model: &Project, name: &str| {
        let by_diff = render(engine, &format!("sync_{name}"), 16 * Q);
        load_project(engine, model, &no_blobs).expect("sync: reload");
        let loaded = render(engine, &format!("sync_{name}_loaded"), 16 * Q);
        assert_same_envelope(&by_diff, &loaded, &format!("sync {name}"));
        // With one oscillator the level repeats to within a fraction of a percent, so a mix or
        // parameter the diff missed shows.
        let energy = |wav: &Wav| wav.envelope().iter().map(|x| x * x).sum::<f64>();
        let ratio = (energy(&by_diff) / energy(&loaded)).sqrt();
        println!("RESULT sync_{name}_level_ratio={ratio:.4}");
        assert!(
            (0.98..1.02).contains(&ratio),
            "sync {name}: level differs by {ratio:.4}"
        );
        by_diff
    };

    // 1. Additions and changes.
    let edits: Vec<Vec<Op>> = vec![
        vec![
            Op::TrackAdd {
                id: t2,
                shell: shell("two"),
                content: TrackContent::default(),
                after: Some(t1),
            },
            Op::DeviceAdd {
                track: t2,
                device: pocket(d2),
                after: None,
            },
            Op::ClipAdd {
                track: t2,
                id: c2,
                clip: clip(4, 4),
            },
        ],
        vec![
            Op::NoteAdd {
                track: t2,
                clip: c2,
                id: n[2],
                note: note(1, 67),
            },
            Op::NoteAdd {
                track: t2,
                clip: c2,
                id: n[3],
                note: note(3, 71),
            },
        ],
        vec![Op::NoteSet {
            track: t1,
            clip: c1,
            id: n[1],
            prop: NoteProp::Pitch(52),
        }],
        vec![Op::ClipSet {
            track: t1,
            id: c1,
            prop: ClipProp::Start(Tick(Q)),
        }],
        vec![Op::TempoPut {
            id: tempo1,
            point: TempoPoint {
                tick: Tick(4 * Q),
                milli_bpm: 180_000,
            },
        }],
        vec![
            Op::TrackSetMix {
                track: t2,
                prop: MixProp::Gain(0.5),
            },
            Op::TrackSetShell {
                id: t2,
                prop: mdaw_model::ShellProp::Name("two, renamed".into()),
            },
        ],
        vec![Op::DeviceSetParam {
            track: t1,
            id: d1,
            param: 11,
            value: Some(800.0),
        }],
    ];
    let mut undo: Vec<Vec<Op>> = Vec::new();
    for (i, ops) in edits.into_iter().enumerate() {
        undo.push(apply(&mut model, ops));
        step(&mut synced, &model, &format!("addition {i}"));
    }
    let after_additions = model.clone();
    let wav = compare(&model, "additions");
    assert!(
        wav.peak_between(2.0, 3.0) > SOUND,
        "sync additions: track two sounds"
    );

    // 2. Removals and moves.
    let edits: Vec<Vec<Op>> = vec![
        vec![Op::NoteRemove {
            track: t2,
            clip: c2,
            id: n[3],
        }],
        vec![Op::TrackMove { id: t2, after: None }],
        vec![
            Op::TrackAdd {
                id: t3,
                shell: shell("three"),
                content: TrackContent::default(),
                after: Some(t2),
            },
            Op::DeviceAdd {
                track: t3,
                device: pocket(d3),
                after: None,
            },
            Op::ClipAdd {
                track: t3,
                id: c3,
                clip: clip(8, 4),
            },
            Op::NoteAdd {
                track: t3,
                clip: c3,
                id: n[4],
                note: note(0, 72),
            },
        ],
        vec![Op::DeviceSetBypass {
            track: t1,
            id: d1,
            bypass: true,
        }],
        vec![Op::TrackMove {
            id: t1,
            after: Some(t2),
        }],
        vec![Op::TempoRemove { id: tempo1 }],
        vec![Op::ClipRemove { track: t2, id: c2 }],
        vec![Op::TrackRemove { id: t1 }],
    ];
    for (i, ops) in edits.into_iter().enumerate() {
        undo.push(apply(&mut model, ops));
        step(&mut synced, &model, &format!("removal {i}"));
    }
    assert!(!engine.has(EntityKind::Track, t1.0.0) && !engine.has(EntityKind::Clip, c2.0.0));
    assert!(engine.has(EntityKind::Track, t3.0.0) && engine.has(EntityKind::Device, d3.0.0));
    let after_removals = model.clone();
    let wav = compare(&model, "removals");
    assert!(
        wav.peak_between(4.0, 4.5) > SOUND,
        "sync removals: track three sounds"
    );
    assert!(
        wav.peak_between(0.0, 3.9) < SILENT,
        "sync removals: the rest is gone"
    );

    // 3. Undo, one step at a time, back to where the removals began.
    let removals = undo.split_off(undo.len() - 8);
    for (i, inverses) in removals.into_iter().rev().enumerate() {
        apply(&mut model, inverses.into_iter().rev().collect());
        step(&mut synced, &model, &format!("undo {i}"));
    }
    assert_eq!(model, after_additions, "undo: the model is back");
    compare(&model, "undone");

    // 4. A rollback: from the state after the removals straight back to the start, and to the
    //    state after the removals again.
    load_project(engine, &after_removals, &no_blobs).expect("sync: load");
    synced = after_removals.clone();
    step(&mut synced, &start, "rollback");
    let wav = compare(&start, "rollback");
    assert!(
        wav.peak_between(0.0, 0.4) > SOUND,
        "rollback: the first note is back"
    );
    step(&mut synced, &after_removals, "forward again");
    compare(&after_removals, "forward_again");

    // 5. One note changed in a project of some size costs one call.
    let mut big = after_additions.clone();
    let mut ops = Vec::new();
    for i in 0..200 {
        ops.push(Op::NoteAdd {
            track: t2,
            clip: c2,
            id: ids_gen.note(),
            note: Note {
                start: Tick(i * 7),
                dur: Tick(5),
                pitch: 40 + (i % 40) as u8,
                vel: 50,
            },
        });
    }
    apply(&mut big, ops);
    load_project(engine, &big, &no_blobs).expect("sync: big load");
    let mut next = big.clone();
    apply(
        &mut next,
        vec![Op::NoteSet {
            track: t1,
            clip: c1,
            id: n[0],
            prop: NoteProp::Pitch(61),
        }],
    );
    let calls = engine.call_count();
    let started = Instant::now();
    sync_project(engine, &big, &next, &no_blobs).expect("sync: one note");
    let (calls, took) = (engine.call_count() - calls, started.elapsed());
    println!(
        "RESULT sync_one_note_calls={calls} sync_one_note_micros={}",
        took.as_micros()
    );
    assert_eq!(calls, 1, "one changed note: one call (notes_put)");
    let calls = engine.call_count();
    sync_project(engine, &next, &next, &no_blobs).expect("sync: nothing");
    assert_eq!(engine.call_count() - calls, 0, "no change: no call");
}

/// Step 8: plugin devices. One whose plugin is not installed is left out and reported; the
/// CLAP test synth, if built, is found by its CLAP id and plays; its parameters follow the model
/// (plain values in the plugin's own range) and its state comes back through the blob resolver.
fn plugins(world: &mut World, ids_gen: &mut IdGen, have_clap: bool) {
    if have_clap {
        let engine = &world.engine;
        engine.plugins_scan("CLAP").expect("scan CLAP");
        let list = engine.plugins().expect("plugin list");
        println!("plugins: {list:?}");
        let synths: Vec<_> = list.iter().filter(|p| p.identifier == TEST_SYNTH).collect();
        assert_eq!(
            synths.len(),
            1,
            "plugins: the test synth listed once under its CLAP id"
        );
        assert_eq!(synths[0].format, "CLAP");
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
    assert!(
        !world.engine.has(EntityKind::Device, missing.0.0),
        "plugins: the missing one is not in the engine"
    );
    if !have_clap {
        return;
    }
    let raw = synth.0.0;
    assert!(world.engine.has(EntityKind::Device, raw), "clap: in the engine");

    // Model tempo is still 240 bpm: 4 beats are 1 s. Level 0.5 (the default), balance 0.
    let wav = render(&world.engine, "clap", 8 * Q);
    let (base_left, base_right) = wav.channel_peaks();
    assert!(base_left > 0.01, "clap: the test synth sounds");
    assert!(
        (base_left / base_right - 1.0).abs() < 0.05,
        "clap: centred ({base_left} / {base_right})"
    );

    let params = world.engine.device_params(raw).expect("clap params");
    println!("clap params: {params:?}");
    let balance = params
        .iter()
        .find(|p| p.id == "1")
        .expect("clap: a balance parameter");
    assert_eq!((balance.min, balance.max), (-1.0, 1.0), "clap: balance range");
    assert!(!balance.text.is_empty(), "clap: balance has a text");
    let text = world
        .engine
        .device_param_text(raw, "0", 0.25)
        .expect("param text");
    println!("clap level text at 0.25: {text}");
    assert!(text.contains("25"), "clap: level text ({text})");
    assert_eq!(
        world
            .engine
            .device_param_text(raw, "99", 0.5)
            .map_err(|e| e.status),
        Err(Status::NotFound),
        "clap: text of an unknown parameter"
    );

    // Level 0.1 (plain = normalised for 0..1): a fifth of the default level.
    let set = |param: u32, value: Option<f64>| Op::DeviceSetParam {
        track,
        id: synth,
        param,
        value,
    };
    let problems = world.op(set(0, Some(0.1)));
    assert!(problems.is_empty(), "clap: level op applied: {problems:?}");
    let quiet = render(&world.engine, "clap_level_low", 8 * Q).channel_peaks().0;
    let ratio = quiet / base_left;
    println!("clap: level 0.1 gives {ratio:.3} of the default level's peak");
    assert!(
        (0.17..0.23).contains(&ratio),
        "clap: level 0.1 is a fifth ({ratio})"
    );

    // Balance -1 (plain), normalised 0: left only. A plain value used as normalised would
    // have been clamped to 0 as well, so -0.5 is checked too: normalised 0.25.
    world.op(set(1, Some(-1.0)));
    let (left, right) = render(&world.engine, "clap_balance_left", 8 * Q).channel_peaks();
    assert!(
        left > 0.01 && right < left * 0.01,
        "clap: balance -1 is left only ({left} / {right})"
    );
    world.op(set(1, Some(-0.5)));
    let value = world.engine.device_get_param(raw, "1").expect("balance");
    assert!(
        (value - 0.25).abs() < 1e-4,
        "clap: balance -0.5 is normalised 0.25 ({value})"
    );
    let (left, right) = render(&world.engine, "clap_balance_half", 8 * Q).channel_peaks();
    assert!(
        (right / left - 0.5).abs() < 0.05,
        "clap: balance -0.5 halves the right ({left} / {right})"
    );
    // Out of range in the model: clamped to the plugin's range.
    world.op(set(1, Some(-7.0)));
    let value = world.engine.device_get_param(raw, "1").expect("balance");
    assert!(value.abs() < 1e-6, "clap: balance below range clamped ({value})");

    // A model with stored parameter values, loaded whole: the values are applied.
    let incremental = render(&world.engine, "clap_incremental", 8 * Q).channel_peaks();
    let problems = world.load();
    assert!(
        problems
            .iter()
            .all(|p| matches!(p, Problem::DeviceSkipped { device, .. } if *device == missing)),
        "clap: reload problems: {problems:?}"
    );
    let reloaded = render(&world.engine, "clap_reloaded", 8 * Q).channel_peaks();
    println!("clap: incremental {incremental:?}, reloaded {reloaded:?}");
    assert!(
        (reloaded.0 / incremental.0 - 1.0).abs() < 0.05 && reloaded.1 < reloaded.0 * 0.01,
        "clap: the loaded params match the incremental ones"
    );
    let level = world.engine.device_get_param(raw, "0").expect("level");
    assert!((level - 0.1).abs() < 1e-4, "clap: level loaded ({level})");

    // Back to the defaults (value None): level 0.5, centred.
    world.op(set(0, None));
    world.op(set(1, None));
    let (left, right) = render(&world.engine, "clap_defaults", 8 * Q).channel_peaks();
    assert!(
        (left / base_left - 1.0).abs() < 0.05 && (right / base_right - 1.0).abs() < 0.05,
        "clap: back to the default level ({left} / {right} vs {base_left} / {base_right})"
    );

    // A state, through the resolver.
    world
        .engine
        .device_set_param(raw, "0", 0.2)
        .expect("clap set level");
    render(&world.engine, "clap_level", 8 * Q); // the value reaches the plugin with a processed block
    let state = world.engine.device_get_state(raw);
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

    // Back to no state: the plugin as it starts (level 0.5), in place.
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
    assert!(
        world.engine.has(EntityKind::Device, raw),
        "clap: the reset device is still there"
    );
    let (left, _) = render(&world.engine, "clap_reset", 8 * Q).channel_peaks();
    assert!(
        (left / base_left - 1.0).abs() < 0.05,
        "clap: the reset synth sounds at the default level ({left} vs {base_left})"
    );

    // A stored parameter value stays on top of a reset, as on a load.
    world.op(set(0, Some(0.1)));
    world.op(Op::DeviceSetState {
        track,
        id: synth,
        state: None,
    });
    let level = world.engine.device_get_param(raw, "0").expect("clap get level");
    assert!(
        (level - 0.1).abs() < 1e-4,
        "clap: model value kept over the reset ({level})"
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
