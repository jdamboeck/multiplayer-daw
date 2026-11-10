//! Pins the exact output of the built-in synth and the offline renderer.
//!
//! If `golden_hash` fails after a deliberate change to the sound, bump `pocket::DSP_VERSION`
//! and update the hash. If it fails on another platform or toolchain, determinism is broken and
//! that is the bug.

use mdaw_dsp::pocket::{DSP_VERSION, PARAMS, param};
use mdaw_dsp::{render_offline, render_with_block};
use mdaw_model::*;

const SR: u32 = 48_000;

struct Fixture {
    project: Project,
    lead: TrackId,
    pad: TrackId,
}

fn pocket(ids: &mut IdGen, params: &[(usize, f64)]) -> Device {
    let mut device = Device::new(
        ids.device(),
        DeviceKind::Builtin {
            kind: BuiltinKind::Pocket,
            dsp_version: DSP_VERSION,
        },
    );
    for (index, value) in params {
        device.params.insert(PARAMS[*index].id, *value);
    }
    device
}

fn note(start_16ths: i64, len_16ths: i64, pitch: u8, vel: u8) -> Note {
    Note {
        start: Tick(start_16ths * PPQ / 4),
        dur: Tick(len_16ths * PPQ / 4),
        pitch,
        vel,
    }
}

/// Four bars: an arpeggio with a tempo change in the middle, and a pad that plays more notes
/// than the synth has voices.
fn fixture() -> Fixture {
    let mut ids = IdGen::new(ActorId(1));
    let (lead, pad) = (ids.track(), ids.track());
    let (lead_clip, pad_clip) = (ids.clip(), ids.clip());
    let shell = |name: &str| TrackShell {
        name: name.into(),
        color: 0,
    };
    let mut ops = vec![
        Op::TempoPut {
            id: ids.tempo(),
            point: TempoPoint {
                tick: Tick::ZERO,
                milli_bpm: 126_000,
            },
        },
        Op::TempoPut {
            id: ids.tempo(),
            point: TempoPoint {
                tick: Tick::bars(2),
                milli_bpm: 97_500,
            },
        },
        Op::MasterSetGain { gain: 0.9 },
        Op::TrackAdd {
            id: lead,
            shell: shell("Lead"),
            content: TrackContent::default(),
            after: None,
        },
        Op::TrackAdd {
            id: pad,
            shell: shell("Pad"),
            content: TrackContent::default(),
            after: Some(lead),
        },
        Op::TrackSetMix {
            track: lead,
            prop: MixProp::Pan(-0.3),
        },
        Op::TrackSetMix {
            track: pad,
            prop: MixProp::Gain(0.6),
        },
        Op::TrackSetMix {
            track: pad,
            prop: MixProp::Pan(0.45),
        },
        Op::DeviceAdd {
            track: lead,
            device: pocket(
                &mut ids,
                &[
                    (param::OSC2_WAVE, 1.0),
                    (param::OSC2_SEMI, 12.0),
                    (param::PULSE_WIDTH, 0.3),
                    (param::CUTOFF, 900.0),
                    (param::RESONANCE, 0.65),
                    (param::FILTER_ENV, 4.0),
                    (param::AMP_RELEASE, 0.12),
                ],
            ),
            after: None,
        },
        Op::DeviceAdd {
            track: pad,
            device: pocket(
                &mut ids,
                &[
                    (param::OSC1_WAVE, 2.0),
                    (param::OSC2_FINE, -11.0),
                    (param::NOISE, 0.05),
                    (param::AMP_ATTACK, 0.4),
                    (param::AMP_RELEASE, 1.5),
                    (param::CUTOFF, 1800.0),
                    (param::KEY_TRACK, 1.0),
                ],
            ),
            after: None,
        },
        Op::ClipAdd {
            track: lead,
            id: lead_clip,
            clip: Clip {
                start: Tick::ZERO,
                length: Tick::bars(4),
                name: "arp".into(),
                notes: Default::default(),
            },
        },
        Op::ClipAdd {
            track: pad,
            id: pad_clip,
            clip: Clip {
                start: Tick::bars(1),
                length: Tick::bars(3),
                name: "pad".into(),
                notes: Default::default(),
            },
        },
    ];
    let arp = [45u8, 57, 60, 64, 69, 64, 60, 57];
    for step in 0..64i64 {
        let pitch = arp[step as usize % arp.len()] + if step >= 32 { 3 } else { 0 };
        let vel = 70 + ((step * 37) % 57) as u8;
        ops.push(Op::NoteAdd {
            track: lead,
            clip: lead_clip,
            id: ids.note(),
            note: note(step, 1, pitch, vel),
        });
    }
    // 20 notes at once: four more than there are voices.
    for i in 0..20i64 {
        let pitch = 36 + (i * 5 % 48) as u8;
        ops.push(Op::NoteAdd {
            track: pad,
            clip: pad_clip,
            id: ids.note(),
            note: note(i % 4, 30 + i, pitch, 60 + (i * 3) as u8),
        });
    }
    let mut project = Project::default();
    for op in &ops {
        assert_eq!(op.validate(), Ok(()));
        assert!(apply(&mut project, op).is_some());
    }
    Fixture { project, lead, pad }
}

fn hash(samples: &[f32]) -> String {
    let mut hasher = blake3::Hasher::new();
    for sample in samples {
        hasher.update(&sample.to_le_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0, |m, s| m.max(s.abs()))
}

fn whole() -> TickRange {
    TickRange::new(Tick::ZERO, Tick::bars(4))
}

#[test]
fn golden_hash() {
    let f = fixture();
    let audio = render_offline(&f.project, None, whole(), SR);
    // Two bars at 126 bpm, two at 97.5 bpm, one second of tail.
    let seconds = 8.0 * 60.0 / 126.0 + 8.0 * 60.0 / 97.5 + 1.0;
    assert_eq!(audio.len(), (seconds * SR as f64).round() as usize * 2);
    assert!(audio.iter().all(|s| s.is_finite()));
    let peak = peak(&audio);
    assert!(peak > 0.1 && peak < 1.0, "peak {peak}");
    assert_eq!(
        hash(&audio),
        "1198a327b5144d6baed16d50e92d60d9f634f15ce4dc8c40f038b9948223019e",
        "the built-in synth no longer produces the pinned output"
    );
}

#[test]
fn block_size_does_not_change_the_output() {
    let f = fixture();
    let reference = hash(&render_offline(&f.project, None, whole(), SR));
    for block in [1, 13, 64, 512] {
        let audio = render_with_block(&f.project, None, whole(), SR, block);
        assert_eq!(hash(&audio), reference, "block size {block}");
    }
}

#[test]
fn tracks_render_separately_and_sum_to_the_mix() {
    let f = fixture();
    let mut silent_master = f.project.clone();
    apply(&mut silent_master, &Op::MasterSetGain { gain: 1.0 });
    let mix = render_offline(&silent_master, None, whole(), SR);
    let lead = render_offline(&silent_master, Some(&[f.lead]), whole(), SR);
    let pad = render_offline(&silent_master, Some(&[f.pad]), whole(), SR);
    assert_ne!(hash(&lead), hash(&pad));
    let worst = mix
        .iter()
        .zip(lead.iter().zip(&pad))
        .map(|(m, (l, p))| (m - (l + p)).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 1e-5, "mix differs from the sum of its tracks by {worst}");

    // The lead is panned left, the pad right.
    let energy = |audio: &[f32], ch: usize| -> f32 { audio.chunks_exact(2).map(|f| f[ch] * f[ch]).sum() };
    assert!(energy(&lead, 0) > energy(&lead, 1) * 1.5);
    assert!(energy(&pad, 1) > energy(&pad, 0) * 1.5);
}

#[test]
fn a_partial_render_matches_the_full_one() {
    let f = fixture();
    let bar4 = TickRange::new(Tick::bars(3), Tick::bars(4));

    // The pad notes start in bar 2, more than the pre-roll before bar 4, and are held. They
    // must sound from the first sample on.
    let pad = render_offline(&f.project, Some(&[f.pad]), bar4, SR);
    assert!(peak(&pad[..2000]) > 0.01, "held notes are missing at the start");

    // The lead's notes are short, so everything that matters lies inside the pre-roll and the
    // partial render equals the same stretch of the full one.
    let part = render_offline(&f.project, Some(&[f.lead]), bar4, SR);
    let full = render_offline(&f.project, Some(&[f.lead]), whole(), SR);
    let offset = full.len() - part.len();
    let worst = part
        .iter()
        .zip(&full[offset..])
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(
        worst < 1e-3,
        "partial render differs from the full one by {worst}"
    );
}

#[test]
fn mute_and_empty_ranges() {
    let f = fixture();
    let mut muted = f.project.clone();
    apply(
        &mut muted,
        &Op::TrackSetMix {
            track: f.lead,
            prop: MixProp::Mute(true),
        },
    );
    assert_eq!(peak(&render_offline(&muted, Some(&[f.lead]), whole(), SR)), 0.0);

    // A range past the end of the song is silence plus tail.
    let late = TickRange::new(Tick::bars(40), Tick::bars(41));
    let audio = render_offline(&f.project, None, late, SR);
    assert_eq!(peak(&audio), 0.0);

    // An empty range still yields the tail.
    let empty = TickRange::new(Tick::bars(1), Tick::bars(1));
    assert_eq!(render_offline(&f.project, None, empty, SR).len(), SR as usize * 2);
}

#[test]
fn renders_faster_than_real_time() {
    let f = fixture();
    let started = std::time::Instant::now();
    let audio = render_offline(&f.project, None, whole(), SR);
    let took = started.elapsed().as_secs_f64();
    let seconds = audio.len() as f64 / 2.0 / SR as f64;
    eprintln!(
        "rendered {seconds:.1} s in {took:.3} s ({:.0}x real time)",
        seconds / took
    );
    assert!(
        took < seconds / 4.0,
        "too slow for the audio thread: {took:.2} s for {seconds:.1} s"
    );
}
