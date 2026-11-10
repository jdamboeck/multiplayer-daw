//! Property tests for the op algebra: totality, inverses, determinism across the wire.

use mdaw_model::*;
use proptest::prelude::*;

const ACTOR: ActorId = ActorId(7);

// Small ID pools, so random ops hit existing entities often.
fn track_id() -> impl Strategy<Value = TrackId> {
    (0u32..2).prop_map(|n| TrackId(EntityId::new(ACTOR, n)))
}
fn clip_id() -> impl Strategy<Value = ClipId> {
    (10u32..12).prop_map(|n| ClipId(EntityId::new(ACTOR, n)))
}
fn note_id() -> impl Strategy<Value = NoteId> {
    (20u32..24).prop_map(|n| NoteId(EntityId::new(ACTOR, n)))
}
fn device_id() -> impl Strategy<Value = DeviceId> {
    (30u32..33).prop_map(|n| DeviceId(EntityId::new(ACTOR, n)))
}
fn tempo_id() -> impl Strategy<Value = TempoId> {
    (40u32..42).prop_map(|n| TempoId(EntityId::new(ACTOR, n)))
}

fn tick() -> impl Strategy<Value = Tick> {
    (0i64..TICKS_PER_BAR * 16).prop_map(Tick)
}
fn length() -> impl Strategy<Value = Tick> {
    (1i64..TICKS_PER_BAR * 4).prop_map(Tick)
}
fn name() -> impl Strategy<Value = String> {
    "[a-c]{0,3}"
}
fn gain() -> impl Strategy<Value = f32> {
    (0u32..=16).prop_map(|n| n as f32 * 0.25)
}
fn pan() -> impl Strategy<Value = f32> {
    (-4i32..=4).prop_map(|n| n as f32 * 0.25)
}

fn note() -> impl Strategy<Value = Note> {
    (tick(), length(), 60u8..=63, (1u8..=4).prop_map(|v| v * 30)).prop_map(|(start, dur, pitch, vel)| Note {
        start,
        dur,
        pitch,
        vel,
    })
}

fn clip() -> impl Strategy<Value = Clip> {
    (
        tick(),
        length(),
        name(),
        prop::collection::vec((note_id(), note()), 0..4),
    )
        .prop_map(|(start, length, name, notes)| Clip {
            start,
            length,
            name,
            notes: notes.into_iter().collect(),
        })
}

fn device() -> impl Strategy<Value = Device> {
    let kind = prop_oneof![
        Just(DeviceKind::Builtin {
            kind: BuiltinKind::Pocket,
            dsp_version: 1
        }),
        prop::option::of(any::<[u8; 32]>().prop_map(BlobHash)).prop_map(|state| DeviceKind::Clap {
            plugin_id: "test.plugin".into(),
            state
        }),
    ];
    (
        device_id(),
        kind,
        prop::collection::vec((0u32..4, -2i32..=2), 0..3),
        any::<bool>(),
    )
        .prop_map(|(id, kind, params, bypass)| Device {
            id,
            kind,
            params: params.into_iter().map(|(k, v)| (k, v as f64 * 0.5)).collect(),
            bypass,
        })
}

fn content() -> impl Strategy<Value = TrackContent> {
    (
        gain(),
        pan(),
        any::<bool>(),
        prop::collection::vec((clip_id(), clip()), 0..3),
        prop::collection::vec(device(), 0..4),
    )
        .prop_map(|(gain, pan, mute, clips, devices)| {
            // Device IDs are unique within a track.
            let mut seen = std::collections::BTreeSet::new();
            TrackContent {
                gain,
                pan,
                mute,
                clips: clips
                    .into_iter()
                    .map(|(id, c)| (id, std::sync::Arc::new(c)))
                    .collect(),
                devices: devices.into_iter().filter(|d| seen.insert(d.id)).collect(),
            }
        })
}

fn op() -> impl Strategy<Value = Op> {
    let shell = (name(), 0u32..4).prop_map(|(name, color)| TrackShell { name, color });
    let tempo_point = (tick(), 20u32..=400).prop_map(|(tick, bpm)| TempoPoint {
        tick,
        milli_bpm: bpm * 1000,
    });
    prop_oneof![
        1 => name().prop_map(|name| Op::ProjectSetName { name }),
        1 => gain().prop_map(|gain| Op::MasterSetGain { gain }),
        2 => (tempo_id(), tempo_point).prop_map(|(id, point)| Op::TempoPut { id, point }),
        1 => tempo_id().prop_map(|id| Op::TempoRemove { id }),
        4 => (track_id(), shell, content(), prop::option::of(track_id()))
            .prop_map(|(id, shell, content, after)| Op::TrackAdd { id, shell, content, after }),
        3 => (track_id(), prop::option::of(track_id())).prop_map(|(id, after)| Op::TrackMove { id, after }),
        1 => (track_id(), name()).prop_map(|(id, n)| Op::TrackSetShell { id, prop: ShellProp::Name(n) }),
        1 => (track_id(), 0u32..4).prop_map(|(id, c)| Op::TrackSetShell { id, prop: ShellProp::Color(c) }),
        1 => track_id().prop_map(|id| Op::TrackRemove { id }),
        1 => (track_id(), gain()).prop_map(|(track, g)| Op::TrackSetMix { track, prop: MixProp::Gain(g) }),
        1 => (track_id(), pan()).prop_map(|(track, p)| Op::TrackSetMix { track, prop: MixProp::Pan(p) }),
        1 => (track_id(), any::<bool>())
            .prop_map(|(track, m)| Op::TrackSetMix { track, prop: MixProp::Mute(m) }),
        6 => (track_id(), clip_id(), clip()).prop_map(|(track, id, clip)| Op::ClipAdd { track, id, clip }),
        2 => (track_id(), clip_id()).prop_map(|(track, id)| Op::ClipRemove { track, id }),
        2 => (track_id(), clip_id(), tick())
            .prop_map(|(track, id, t)| Op::ClipSet { track, id, prop: ClipProp::Start(t) }),
        2 => (track_id(), clip_id(), length())
            .prop_map(|(track, id, l)| Op::ClipSet { track, id, prop: ClipProp::Length(l) }),
        2 => (track_id(), clip_id(), name())
            .prop_map(|(track, id, n)| Op::ClipSet { track, id, prop: ClipProp::Name(n) }),
        10 => (track_id(), clip_id(), note_id(), note())
            .prop_map(|(track, clip, id, note)| Op::NoteAdd { track, clip, id, note }),
        4 => (track_id(), clip_id(), note_id())
            .prop_map(|(track, clip, id)| Op::NoteRemove { track, clip, id }),
        3 => (track_id(), clip_id(), note_id(), tick())
            .prop_map(|(track, clip, id, t)| Op::NoteSet { track, clip, id, prop: NoteProp::Start(t) }),
        3 => (track_id(), clip_id(), note_id(), length())
            .prop_map(|(track, clip, id, d)| Op::NoteSet { track, clip, id, prop: NoteProp::Dur(d) }),
        3 => (track_id(), clip_id(), note_id(), 60u8..=63)
            .prop_map(|(track, clip, id, p)| Op::NoteSet { track, clip, id, prop: NoteProp::Pitch(p) }),
        3 => (track_id(), clip_id(), note_id(), (1u8..=4).prop_map(|v| v * 30))
            .prop_map(|(track, clip, id, v)| Op::NoteSet { track, clip, id, prop: NoteProp::Vel(v) }),
        8 => (track_id(), device(), prop::option::of(device_id()))
            .prop_map(|(track, device, after)| Op::DeviceAdd { track, device, after }),
        3 => (track_id(), device_id()).prop_map(|(track, id)| Op::DeviceRemove { track, id }),
        8 => (track_id(), device_id(), prop::option::of(device_id()))
            .prop_map(|(track, id, after)| Op::DeviceMove { track, id, after }),
        6 => (track_id(), device_id(), 0u32..4, prop::option::of(-2i32..=2)).prop_map(
            |(track, id, param, v)| Op::DeviceSetParam { track, id, param, value: v.map(|v| v as f64 * 0.5) }
        ),
        4 => (track_id(), device_id(), any::<bool>())
            .prop_map(|(track, id, bypass)| Op::DeviceSetBypass { track, id, bypass }),
        6 => (track_id(), device_id(), prop::option::of(any::<[u8; 32]>().prop_map(BlobHash)))
            .prop_map(|(track, id, state)| Op::DeviceSetState { track, id, state }),
    ]
}

fn ops(max: usize) -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(op(), 0..max)
}

fn build(ops: &[Op]) -> Project {
    let mut project = Project::default();
    apply_all(&mut project, ops);
    project
}

/// Invariants that must hold after any op sequence.
fn check_invariants(project: &Project) {
    let order: Vec<TrackId> = project.track_order.iter().copied().collect();
    let mut sorted = order.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), order.len(), "track order has duplicates");
    let keys: Vec<TrackId> = project.tracks.keys().copied().collect();
    assert_eq!(sorted, keys, "track order and track map disagree");
    for track in project.tracks.values() {
        let mut ids: Vec<DeviceId> = track.content.devices.iter().map(|d| d.id).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate device ids on a track");
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// Every op along a random history either changes nothing and reports that, or changes the
    /// document and returns an inverse that restores it exactly.
    #[test]
    fn inverse_restores_the_document(history in ops(80)) {
        let mut project = Project::default();
        for op in &history {
            let before = project.clone();
            match apply(&mut project, op) {
                None => prop_assert_eq!(&project, &before, "no-op changed the document: {:?}", op),
                Some(inverse) => {
                    check_invariants(&project);
                    prop_assert_ne!(&project, &before, "reported a change, changed nothing: {:?}", op);
                    let mut undone = project.clone();
                    let redo = apply(&mut undone, &inverse);
                    prop_assert_eq!(&undone, &before, "inverse of {:?} is wrong: {:?}", op, inverse);
                    // Inverting the inverse replays the original change.
                    prop_assert!(redo.is_some());
                    apply(&mut undone, &redo.unwrap());
                    prop_assert_eq!(&undone, &project);
                }
            }
        }
    }

    /// A whole transaction is undone by the inverses `apply_all` returns.
    #[test]
    fn apply_all_returns_a_working_undo(setup in ops(30), txn in ops(12)) {
        let before = build(&setup);
        let mut project = before.clone();
        let undo = apply_all(&mut project, &txn);
        check_invariants(&project);
        apply_all(&mut project, &undo);
        prop_assert_eq!(&project, &before);
    }

    /// A replica that receives the ops over the wire ends in the same state, and a document
    /// survives serialisation unchanged.
    #[test]
    fn replicas_converge_across_the_wire(history in ops(60)) {
        let local = build(&history);
        let bytes = postcard::to_stdvec(&history).unwrap();
        let received: Vec<Op> = postcard::from_bytes(&bytes).unwrap();
        prop_assert_eq!(&received, &history);
        let remote = build(&received);
        prop_assert_eq!(&remote, &local);

        let snapshot = postcard::to_stdvec(&local).unwrap();
        let restored: Project = postcard::from_bytes(&snapshot).unwrap();
        prop_assert_eq!(&restored, &local);
        prop_assert_eq!(postcard::to_stdvec(&restored).unwrap(), snapshot);
    }

    /// Everything the generators produce is in range, and the metadata accessors are total.
    #[test]
    fn generated_ops_are_valid(op in op()) {
        prop_assert_eq!(op.validate(), Ok(()));
        let entity = op.entity();
        match op.scope() {
            Scope::Project => {}
            Scope::TrackStructure(t) | Scope::TrackContent(t) => prop_assert_eq!(entity.track(), Some(t)),
        }
        let _ = (op.prop(), op.is_removal(), op.adds_clap_device(), op.blobs());
    }

    /// The diff reports exactly the tracks whose content changed.
    #[test]
    fn affected_ranges_match_changed_tracks(setup in ops(30), txn in ops(10)) {
        let before = build(&setup);
        let mut after = before.clone();
        apply_all(&mut after, &txn);
        let affected = affected_ranges(&before, &txn);

        let mut seen = std::collections::BTreeSet::new();
        for a in &affected {
            prop_assert!(seen.insert(a.track), "track listed twice");
            if let Some(r) = a.range { prop_assert!(!r.is_empty()); }
        }
        let ids: std::collections::BTreeSet<TrackId> =
            before.tracks.keys().chain(after.tracks.keys()).copied().collect();
        for id in ids {
            let content = |p: &Project| p.track(id).map(|t| t.content.clone());
            if content(&before) != content(&after) {
                prop_assert!(seen.contains(&id), "{:?} changed but was not reported", id);
            }
        }
    }
}

/// Guards the generators: if random histories stopped reaching the interesting ops, the
/// properties above would pass without testing anything.
#[test]
fn generators_reach_every_kind_of_change() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
    let rng = TestRng::deterministic_rng(RngAlgorithm::ChaCha);
    let mut runner = TestRunner::new_with_rng(Config::default(), rng);
    let mut effective = std::collections::BTreeMap::<String, usize>::new();
    let mut total = 0usize;
    for _ in 0..300 {
        let history = ops(80).new_tree(&mut runner).unwrap().current();
        let mut project = Project::default();
        for op in &history {
            total += 1;
            if apply(&mut project, op).is_some() {
                let debug = format!("{op:?}");
                let kind = debug.split([' ', '{']).next().unwrap().to_string();
                *effective.entry(kind).or_default() += 1;
            }
        }
    }
    let kinds = [
        "ProjectSetName",
        "MasterSetGain",
        "TempoPut",
        "TempoRemove",
        "TrackAdd",
        "TrackMove",
        "TrackSetShell",
        "TrackRemove",
        "TrackSetMix",
        "ClipAdd",
        "ClipRemove",
        "ClipSet",
        "NoteAdd",
        "NoteRemove",
        "NoteSet",
        "DeviceAdd",
        "DeviceRemove",
        "DeviceMove",
        "DeviceSetParam",
        "DeviceSetBypass",
        "DeviceSetState",
    ];
    for kind in kinds {
        let hits = effective.get(kind).copied().unwrap_or(0);
        assert!(
            hits >= 20,
            "only {hits} effective {kind} ops in {total}: {effective:?}"
        );
    }
}

#[test]
fn track_order_semantics() {
    let mut ids = IdGen::new(ActorId(1));
    let (a, b, c) = (ids.track(), ids.track(), ids.track());
    let add = |id, after| Op::TrackAdd {
        id,
        shell: TrackShell {
            name: String::new(),
            color: 0,
        },
        content: TrackContent::default(),
        after,
    };
    let mut p = Project::default();
    apply(&mut p, &add(a, None));
    apply(&mut p, &add(b, Some(a)));
    apply(&mut p, &add(c, None)); // no anchor: first
    assert_eq!(p.track_order.iter().copied().collect::<Vec<_>>(), [c, a, b]);

    // An anchor that does not exist puts the track last.
    let ghost = ids.track();
    apply(
        &mut p,
        &Op::TrackMove {
            id: c,
            after: Some(ghost),
        },
    );
    assert_eq!(p.track_order.iter().copied().collect::<Vec<_>>(), [a, b, c]);

    // Adding an existing id is a no-op, as is touching a missing track.
    assert_eq!(apply(&mut p, &add(a, None)), None);
    assert_eq!(apply(&mut p, &Op::TrackRemove { id: ghost }), None);
}

#[test]
fn removing_a_clip_and_undoing_brings_back_its_notes() {
    let mut ids = IdGen::new(ActorId(1));
    let (track, clip, n1) = (ids.track(), ids.clip(), ids.note());
    let mut p = Project::default();
    let setup = [
        Op::TrackAdd {
            id: track,
            shell: TrackShell {
                name: "Lead".into(),
                color: 1,
            },
            content: TrackContent::default(),
            after: None,
        },
        Op::ClipAdd {
            track,
            id: clip,
            clip: Clip {
                start: Tick::bars(2),
                length: Tick::bars(1),
                name: "A".into(),
                notes: Default::default(),
            },
        },
        Op::NoteAdd {
            track,
            clip,
            id: n1,
            note: Note {
                start: Tick::beats(1),
                dur: Tick::beats(1),
                pitch: 60,
                vel: 100,
            },
        },
    ];
    apply_all(&mut p, &setup);
    let with_note = p.clone();

    let undo = apply_all(&mut p, &[Op::ClipRemove { track, id: clip }]);
    assert!(p.track(track).unwrap().content.clips.is_empty());
    apply_all(&mut p, &undo);
    assert_eq!(p, with_note);

    // The diff of adding that note is the note's absolute span.
    let before = build(&setup[..2]);
    let affected = affected_ranges(&before, &setup[2..]);
    let start = Tick::bars(2) + Tick::beats(1);
    assert_eq!(
        affected,
        [Affected {
            track,
            range: Some(TickRange::new(start, start + Tick::beats(1)))
        }]
    );
    // ... and the sample covers the bar around it.
    let listen = listening_range(&p.track(track).unwrap().content, affected[0].range).unwrap();
    assert_eq!(listen, TickRange::new(Tick::bars(2), Tick::bars(3)));
    // A mix change has no range; the sample starts at the first bar with notes.
    let mix = [Op::TrackSetMix {
        track,
        prop: MixProp::Gain(0.5),
    }];
    let affected = affected_ranges(&p, &mix);
    assert_eq!(affected, [Affected { track, range: None }]);
    let listen = listening_range(&p.track(track).unwrap().content, None).unwrap();
    assert_eq!(listen, TickRange::new(Tick::bars(2), Tick::bars(6)));
}

#[test]
fn validation_rejects_out_of_range_values() {
    let mut ids = IdGen::new(ActorId(1));
    let (track, clip, note) = (ids.track(), ids.clip(), ids.note());
    let bad = [
        Op::MasterSetGain { gain: f32::NAN },
        Op::MasterSetGain { gain: -0.1 },
        Op::TrackSetMix {
            track,
            prop: MixProp::Pan(1.5),
        },
        Op::TempoPut {
            id: ids.tempo(),
            point: TempoPoint {
                tick: Tick(0),
                milli_bpm: 1,
            },
        },
        Op::TempoPut {
            id: ids.tempo(),
            point: TempoPoint {
                tick: Tick(-1),
                milli_bpm: 120_000,
            },
        },
        Op::ClipSet {
            track,
            id: clip,
            prop: ClipProp::Length(Tick(0)),
        },
        Op::NoteSet {
            track,
            clip,
            id: note,
            prop: NoteProp::Pitch(128),
        },
        Op::NoteSet {
            track,
            clip,
            id: note,
            prop: NoteProp::Vel(0),
        },
        Op::DeviceSetParam {
            track,
            id: ids.device(),
            param: 0,
            value: Some(f64::INFINITY),
        },
        Op::ProjectSetName {
            name: "x".repeat(129),
        },
    ];
    for op in bad {
        assert!(op.validate().is_err(), "{op:?} should be invalid");
    }
}
