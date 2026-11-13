//! Drives the engine without an audio device: `pump` runs exactly the code the device callback
//! runs, under the allocation guard.

use mdaw_dsp::pocket::{DSP_VERSION, PARAMS, param};
use mdaw_dsp::render_offline;
use mdaw_engine::Engine;
use mdaw_engine::alloc_guard::{self, GuardedAlloc};
use mdaw_model::*;

#[global_allocator]
static ALLOC: GuardedAlloc = GuardedAlloc;

const SR: u32 = 48_000;
/// One beat at 120 bpm.
const BEAT: usize = SR as usize / 2;

struct Song {
    project: Project,
    ids: IdGen,
    track: TrackId,
    clip: ClipId,
    device: DeviceId,
}

impl Song {
    /// One track, one four-bar clip, notes given as (start beat, length in beats, pitch).
    fn new(notes: &[(i64, i64, u8)]) -> Song {
        let mut ids = IdGen::new(ActorId(7));
        let (track, clip, device) = (ids.track(), ids.clip(), ids.device());
        let mut song = Song {
            project: Project::default(),
            ids,
            track,
            clip,
            device,
        };
        song.apply(Op::TrackAdd {
            id: track,
            shell: TrackShell {
                name: "t".into(),
                color: 0,
            },
            content: TrackContent::default(),
            after: None,
        });
        song.apply(Op::DeviceAdd {
            track,
            device: Device::new(
                device,
                DeviceKind::Builtin {
                    kind: BuiltinKind::Pocket,
                    dsp_version: DSP_VERSION,
                },
            ),
            after: None,
        });
        song.apply(Op::ClipAdd {
            track,
            id: clip,
            clip: Clip {
                start: Tick::ZERO,
                length: Tick::bars(4),
                name: String::new(),
                notes: Default::default(),
            },
        });
        for &(start, len, pitch) in notes {
            song.note(start, len, pitch);
        }
        song
    }

    fn apply(&mut self, op: Op) {
        assert_eq!(op.validate(), Ok(()));
        assert!(apply(&mut self.project, &op).is_some(), "{op:?} did nothing");
    }

    fn note(&mut self, start: i64, len: i64, pitch: u8) -> NoteId {
        let id = self.ids.note();
        let note = Note {
            start: Tick::beats(start),
            dur: Tick::beats(len),
            pitch,
            vel: 100,
        };
        self.apply(Op::NoteAdd {
            track: self.track,
            clip: self.clip,
            id,
            note,
        });
        id
    }

    fn param(&mut self, index: usize, value: f64) {
        let op = Op::DeviceSetParam {
            track: self.track,
            id: self.device,
            param: PARAMS[index].id,
            value: Some(value),
        };
        self.apply(op);
    }
}

fn engine(song: &Song) -> Engine {
    let mut engine = Engine::manual(SR, false);
    engine.set_project(&song.project);
    engine
}

fn pump(engine: &mut Engine, frames: usize) -> Vec<f32> {
    engine.pump(frames).expect("manual engine")
}

fn peak(audio: &[f32]) -> f32 {
    audio.iter().fold(0.0, |m, s| m.max(s.abs()))
}

fn no_allocations() {
    assert_eq!(
        alloc_guard::violations(),
        0,
        "the audio thread allocated or freed memory"
    );
}

#[test]
fn playback_is_the_offline_render() {
    let song = Song::new(&[(0, 1, 60), (1, 1, 64), (2, 2, 67), (3, 1, 72)]);
    let mut engine = engine(&song);
    assert_eq!(peak(&pump(&mut engine, 1000)), 0.0, "stopped and silent");
    assert_eq!(engine.position(), 0.0);

    engine.play();
    assert!(engine.is_playing());
    let mut live = Vec::new();
    // Device buffers of awkward and changing sizes, some larger than the engine's block.
    for frames in [441, 1, 2048, 37, 513].into_iter().cycle() {
        if live.len() >= BEAT * 8 * 2 {
            break;
        }
        live.extend(pump(&mut engine, frames));
    }
    let offline = render_offline(&song.project, None, TickRange::new(Tick::ZERO, Tick::bars(4)), SR);
    assert!(peak(&live) > 0.05);
    assert_eq!(
        live[..BEAT * 8 * 2],
        offline[..BEAT * 8 * 2],
        "live and offline output differ"
    );

    let expected = live.len() as f64 / 2.0 / BEAT as f64 * PPQ as f64;
    assert!(
        (engine.position() - expected).abs() < 1.0,
        "{} vs {expected}",
        engine.position()
    );
    assert!(engine.take_peak()[0] > 0.05);
    assert_eq!(engine.take_peak(), [0.0, 0.0], "the peak resets when read");
    no_allocations();
}

#[test]
fn stop_releases_notes_and_holds_the_position() {
    let song = Song::new(&[(0, 4, 60)]);
    let mut engine = engine(&song);
    engine.play();
    pump(&mut engine, BEAT);
    engine.stop();
    assert!(!engine.is_playing());
    let tail = pump(&mut engine, SR as usize * 2);
    assert!(peak(&tail[..2000]) > 0.01, "the release is audible");
    assert_eq!(peak(&tail[SR as usize * 2..]), 0.0, "and ends");
    assert!((engine.position() - PPQ as f64).abs() < 1.0);

    // Starting again in the middle of the note makes it sound again.
    engine.play();
    assert!(peak(&pump(&mut engine, 2000)) > 0.01);
    no_allocations();
}

#[test]
fn seek_jumps_and_chases() {
    let song = Song::new(&[(8, 4, 60)]);
    let mut engine = engine(&song);
    engine.seek(Tick::beats(10));
    assert_eq!(
        engine.position(),
        Tick::beats(10).0 as f64,
        "visible before the audio thread ran"
    );
    engine.play();
    let audio = pump(&mut engine, 2000);
    assert!(
        peak(&audio) > 0.01,
        "the note that started at beat 8 is heard from beat 10"
    );
    assert!(engine.position() > Tick::beats(10).0 as f64);

    engine.seek(Tick::ZERO);
    pump(&mut engine, SR as usize);
    assert_eq!(
        peak(&pump(&mut engine, 1000)),
        0.0,
        "nothing plays in the first bars"
    );
    no_allocations();
}

#[test]
fn the_loop_wraps() {
    let song = Song::new(&[(0, 1, 60), (5, 1, 72)]);
    let mut engine = engine(&song);
    engine.set_loop(Some(TickRange::new(Tick::ZERO, Tick::bars(1))));
    engine.play();
    let first = pump(&mut engine, BEAT * 4);
    let second = pump(&mut engine, BEAT * 4);
    let third = pump(&mut engine, BEAT * 2);
    assert!(engine.position() < Tick::bars(1).0 as f64);
    assert!((engine.position() - Tick::beats(2).0 as f64).abs() < 1.0);
    // The note on beat 1 plays in every pass, the one on beat 6 never.
    assert!(peak(&first[..BEAT]) > 0.05 && peak(&second[..BEAT]) > 0.05 && peak(&third[..BEAT]) > 0.05);
    assert_eq!(first[..BEAT], second[..BEAT]);
    assert_eq!(peak(&second[BEAT * 4..]), 0.0, "beat 6 lies outside the loop");

    engine.set_loop(None);
    pump(&mut engine, BEAT * 4);
    assert!(
        engine.position() > Tick::bars(1).0 as f64,
        "without a loop playback runs on"
    );
    no_allocations();
}

#[test]
fn a_knob_turn_does_not_cut_notes_but_a_note_edit_does() {
    let mut song = Song::new(&[(0, 8, 60)]);
    let mut engine = engine(&song);
    engine.play();
    pump(&mut engine, BEAT);

    // Many small parameter changes while the note is held, as a dragged knob produces them.
    let mut quietest = f32::MAX;
    for step in 0..50 {
        song.param(param::CUTOFF, 2500.0 + step as f64 * 40.0);
        engine.set_project(&song.project);
        quietest = quietest.min(peak(&pump(&mut engine, 480)));
        engine.tick();
    }
    assert!(
        quietest > 0.02,
        "the held note dropped out during a knob turn ({quietest})"
    );

    // The change is audible: an open filter is louder and brighter than a closed one.
    let open = pump(&mut engine, 4800);
    song.param(param::CUTOFF, 60.0);
    song.param(param::FILTER_ENV, 0.0);
    song.param(param::KEY_TRACK, 0.0);
    engine.set_project(&song.project);
    pump(&mut engine, 4800);
    let closed = pump(&mut engine, 4800);
    let roughness = |a: &[f32]| {
        let frames = a.as_chunks::<2>().0;
        frames
            .iter()
            .zip(frames.iter().skip(1))
            .map(|(x, y)| (x[0] - y[0]).abs())
            .sum::<f32>()
    };
    assert!(roughness(&open) > roughness(&closed) * 3.0);

    // Editing notes on the track ends what is sounding there.
    song.note(12, 1, 72);
    engine.set_project(&song.project);
    pump(&mut engine, SR as usize);
    assert_eq!(peak(&pump(&mut engine, 1000)), 0.0);
    engine.tick();
    no_allocations();
}

#[test]
fn a_tempo_change_keeps_the_musical_position() {
    let mut song = Song::new(&[(0, 1, 60)]);
    let mut engine = engine(&song);
    engine.play();
    pump(&mut engine, BEAT * 3);
    let before = engine.position();
    assert!((before - Tick::beats(3).0 as f64).abs() < 1.0);

    let id = song.ids.tempo();
    song.apply(Op::TempoPut {
        id,
        point: TempoPoint {
            tick: Tick::ZERO,
            milli_bpm: 60_000,
        },
    });
    engine.set_project(&song.project);
    pump(&mut engine, 1);
    assert!(
        (engine.position() - before).abs() < 1.0,
        "{} after, {before} before",
        engine.position()
    );
    // At 60 bpm a beat takes a second.
    pump(&mut engine, SR as usize);
    assert!((engine.position() - Tick::beats(4).0 as f64).abs() < 1.0);
    no_allocations();
}

#[test]
fn live_notes_sound_without_the_transport() {
    let song = Song::new(&[]);
    let mut engine = engine(&song);
    engine.note_on(song.track, 1, 64, 110);
    let held = pump(&mut engine, 4800);
    assert!(peak(&held) > 0.05);
    engine.note_off(song.track, 1);
    pump(&mut engine, SR as usize);
    assert_eq!(peak(&pump(&mut engine, 1000)), 0.0);

    // A note on a track that does not exist is ignored.
    engine.note_on(TrackId(EntityId::new(ActorId(9), 9)), 2, 64, 110);
    assert_eq!(peak(&pump(&mut engine, 1000)), 0.0);

    // Panic ends held notes.
    engine.note_on(song.track, 3, 64, 110);
    pump(&mut engine, 1000);
    engine.panic();
    pump(&mut engine, SR as usize);
    assert_eq!(peak(&pump(&mut engine, 1000)), 0.0);
    assert_eq!(engine.position(), 0.0);
    no_allocations();
}

#[test]
fn live_notes_from_another_thread_reach_the_audio_thread() {
    let song = Song::new(&[]);
    let mut engine = engine(&song);
    let live = engine.live_input();
    std::thread::spawn(move || assert!(live.note_on(song.track, 5, 64, 110)))
        .join()
        .unwrap();
    assert!(peak(&pump(&mut engine, 4800)) > 0.05);
    let live = engine.live_input();
    std::thread::spawn(move || assert!(live.note_off(song.track, 5)))
        .join()
        .unwrap();
    pump(&mut engine, SR as usize);
    assert_eq!(peak(&pump(&mut engine, 1000)), 0.0);
    no_allocations();
}

#[test]
fn a_stalled_audio_thread_does_not_lose_the_latest_document() {
    let mut song = Song::new(&[]);
    let mut engine = engine(&song);
    // A burst of edits while the audio thread is not running at all.
    for beat in 0..200 {
        song.note(beat % 16, 1, 60);
        engine.set_project(&song.project);
        engine.tick();
    }
    for _ in 0..5000 {
        engine.note_on(song.track, 9, 60, 1);
        engine.note_off(song.track, 9);
    }
    // Once it runs again, the newest document arrives.
    for _ in 0..40 {
        pump(&mut engine, 64);
        engine.tick();
    }
    engine.seek(Tick::ZERO);
    engine.play();
    let mut live = Vec::new();
    while live.len() < BEAT * 4 * 2 {
        live.extend(pump(&mut engine, 500));
    }
    pump(&mut engine, 64);
    let offline = render_offline(&song.project, None, TickRange::new(Tick::ZERO, Tick::bars(4)), SR);
    // Earlier live notes have long ended, so the output is the timeline alone.
    let worst = live[..BEAT * 4 * 2]
        .iter()
        .zip(&offline)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(
        worst < 1e-4,
        "the engine is not playing the newest document (off by {worst})"
    );
    no_allocations();
}

/// Where free playback from tick 0 is after `micros`, in ticks at 120 bpm.
fn ticks_after(micros: i64) -> f64 {
    micros as f64 / 1e6 * 2.0 * PPQ as f64
}

#[test]
fn a_real_time_manual_engine_keeps_time() {
    let song = Song::new(&[(0, 8, 60)]);
    let start = mdaw_engine::clock::now_micros();
    let mut engine = Engine::manual(SR, true);
    engine.set_project(&song.project);
    engine.play();
    // Many small steps must not add up to lost time.
    let mut now = start;
    while now < start + 200_000 {
        std::thread::sleep(std::time::Duration::from_micros(700));
        engine.tick();
        now = mdaw_engine::clock::now_micros();
    }
    let expected = ticks_after(now - start);
    // Within a millisecond: the clock was read a moment after the audio was rendered.
    assert!(
        (engine.position() - expected).abs() < ticks_after(1_000),
        "{} vs {expected}",
        engine.position()
    );
}

#[test]
fn the_listening_volume_scales_what_is_heard() {
    let song = Song::new(&[]);
    let heard = |gain: f32| {
        let mut engine = engine(&song);
        engine.set_output_gain(gain);
        engine.note_on(song.track, 1, 64, 110);
        peak(&pump(&mut engine, 4800))
    };
    let (full, half) = (heard(1.0), heard(0.5));
    assert!((half / full - 0.5).abs() < 1e-3, "{full} {half}");
    let mut engine = engine(&song);
    engine.set_output_gain(f32::NAN);
    assert_eq!(engine.output_gain(), 1.0, "nonsense is unity");
    no_allocations();
}
