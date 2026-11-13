//! What the server lets into the log, and what it records as displaced.

use mdaw_model::*;
use mdaw_proto::{RejectReason, Superseded, Txn, TxnId, blob_hash};
use mdaw_server::Sequencer;

const TRACK: TrackId = TrackId(EntityId(1));
const CLIP: ClipId = ClipId(EntityId(2));
const NOTE: NoteId = NoteId(EntityId(3));

const ANN: (ActorId, UserId) = (ActorId(1), UserId(1));
const BEN: (ActorId, UserId) = (ActorId(2), UserId(2));

/// One track with a four-bar clip holding one note.
fn base() -> Project {
    let mut project = Project::default();
    let ops = [
        Op::TrackAdd {
            id: TRACK,
            shell: TrackShell {
                name: "one".into(),
                color: 1,
            },
            content: TrackContent::default(),
            after: None,
        },
        Op::ClipAdd {
            track: TRACK,
            id: CLIP,
            clip: Clip {
                start: Tick::ZERO,
                length: Tick::bars(4),
                name: String::new(),
                notes: Default::default(),
            },
        },
        Op::NoteAdd {
            track: TRACK,
            clip: CLIP,
            id: NOTE,
            note: Note {
                start: Tick::ZERO,
                dur: Tick::beats(1),
                pitch: 60,
                vel: 100,
            },
        },
    ];
    for op in &ops {
        assert!(apply(&mut project, op).is_some());
    }
    project
}

fn pitch(pitch: u8) -> Op {
    Op::NoteSet {
        track: TRACK,
        clip: CLIP,
        id: NOTE,
        prop: NoteProp::Pitch(pitch),
    }
}

fn vel(vel: u8) -> Op {
    Op::NoteSet {
        track: TRACK,
        clip: CLIP,
        id: NOTE,
        prop: NoteProp::Vel(vel),
    }
}

fn clap_device(state: Option<BlobHash>) -> Device {
    Device::new(
        DeviceId(EntityId(9)),
        DeviceKind::Plugin {
            format: PluginFormat::Clap,
            id: "com.example.synth".into(),
            state,
        },
    )
}

/// A plugin state the server holds, and one it does not.
fn stored() -> BlobHash {
    blob_hash(b"uploaded")
}
fn missing() -> BlobHash {
    blob_hash(b"never uploaded")
}
fn has_blob(blob: &BlobHash) -> bool {
    *blob == stored()
}

fn txn(id: u64, base_seq: u64, ops: Vec<Op>) -> Txn {
    Txn {
        id: TxnId::from(u128::from(id)),
        base_seq,
        ops,
    }
}

fn superseded(seq: &mut Sequencer, who: (ActorId, UserId), t: Txn) -> Vec<Superseded> {
    seq.sequence(who.0, who.1, 0, t, has_blob).unwrap().superseded
}

fn note(seq: &Sequencer) -> Note {
    seq.doc().track(TRACK).unwrap().content.clips[&CLIP].notes[&NOTE]
}

#[test]
fn events_get_consecutive_numbers_and_carry_the_transaction() {
    let mut seq = Sequencer::new(base(), 7, vec![]);
    let event = seq
        .sequence(ANN.0, ANN.1, 42, txn(5, 7, vec![pitch(62)]), has_blob)
        .unwrap();
    assert_eq!(
        (event.seq, event.actor, event.user, event.wall_ts),
        (8, ANN.0, ANN.1, 42)
    );
    assert_eq!(event.txn, TxnId::from(5));
    assert_eq!(event.ops, [pitch(62)]);
    assert_eq!(seq.head(), 8);
    assert_eq!(note(&seq).pitch, 62);
    assert_eq!(
        seq.sequence(BEN.0, BEN.1, 0, txn(1, 8, vec![vel(1)]), has_blob)
            .unwrap()
            .seq,
        9
    );
}

#[test]
fn a_refused_transaction_changes_nothing() {
    let mut seq = Sequencer::new(base(), 0, vec![]);
    let before = seq.doc().clone();
    let cases = [
        (txn(1, 0, vec![]), "empty"),
        (txn(1, 1, vec![pitch(61)]), "future base"),
        (txn(1, 0, vec![pitch(61), pitch(200)]), "pitch out of range"),
        (
            txn(
                1,
                0,
                vec![
                    pitch(61),
                    Op::DeviceAdd {
                        track: TRACK,
                        device: clap_device(Some(missing())),
                        after: None,
                    },
                ],
            ),
            "plugin state not uploaded",
        ),
    ];
    for (t, what) in cases {
        assert!(seq.sequence(ANN.0, ANN.1, 0, t, has_blob).is_err(), "{what}");
        assert_eq!(seq.head(), 0, "{what}");
        assert_eq!(seq.doc(), &before, "{what}");
    }
    assert_eq!(
        seq.check(&txn(1, 1, vec![pitch(61)]), has_blob),
        Err(RejectReason::FutureBase)
    );
    let add = |state| {
        txn(
            1,
            0,
            vec![Op::DeviceAdd {
                track: TRACK,
                device: clap_device(state),
                after: None,
            }],
        )
    };
    assert_eq!(
        seq.check(&add(Some(missing())), has_blob),
        Err(RejectReason::MissingBlob(missing()))
    );
    assert_eq!(seq.check(&add(Some(stored())), has_blob), Ok(()));
    assert_eq!(seq.check(&add(None), has_blob), Ok(()));
}

/// A track with a plugin is edited like any other: nothing is locked.
#[test]
fn a_track_with_a_plugin_takes_ordinary_edits() {
    let mut doc = base();
    apply(
        &mut doc,
        &Op::DeviceAdd {
            track: TRACK,
            device: clap_device(None),
            after: None,
        },
    )
    .unwrap();
    let mut seq = Sequencer::new(doc, 0, vec![]);
    let device = DeviceId(EntityId(9));
    let state = |state| Op::DeviceSetState {
        track: TRACK,
        id: device,
        state: Some(state),
    };
    seq.sequence(ANN.0, ANN.1, 0, txn(1, 0, vec![pitch(61)]), has_blob)
        .unwrap();
    assert_eq!(
        seq.sequence(BEN.0, BEN.1, 0, txn(1, 1, vec![state(missing())]), has_blob),
        Err(RejectReason::MissingBlob(missing()))
    );
    seq.sequence(BEN.0, BEN.1, 0, txn(2, 1, vec![state(stored())]), has_blob)
        .unwrap();
    let content = &seq.doc().track(TRACK).unwrap().content;
    assert_eq!(note(&seq).pitch, 61);
    assert!(matches!(
        content.device(device).unwrap().kind,
        DeviceKind::Plugin { state: Some(s), .. } if s == stored()
    ));
}

#[test]
fn a_track_and_its_content_can_arrive_in_one_transaction() {
    let mut seq = Sequencer::new(Project::default(), 0, vec![]);
    let track = TrackId(EntityId(50));
    let ops = vec![
        Op::TrackAdd {
            id: track,
            shell: TrackShell {
                name: "new".into(),
                color: 2,
            },
            content: TrackContent::default(),
            after: None,
        },
        Op::TrackSetMix {
            track,
            prop: MixProp::Gain(0.5),
        },
    ];
    seq.sequence(ANN.0, ANN.1, 0, txn(1, 0, ops), has_blob).unwrap();
    assert_eq!(seq.doc().track(track).unwrap().content.gain, 0.5);
}

#[test]
fn an_unseen_write_by_someone_else_is_recorded_as_superseded() {
    let mut seq = Sequencer::new(base(), 0, vec![]);
    assert!(superseded(&mut seq, ANN, txn(1, 0, vec![pitch(62)])).is_empty());

    // Ben had not seen Ann's edit when he made his.
    let lost = superseded(&mut seq, BEN, txn(1, 0, vec![pitch(64), vel(90)]));
    assert_eq!(
        lost,
        [Superseded {
            entity: EntityRef::Note(TRACK, CLIP, NOTE),
            prop: PropKey::Pitch,
            lost: pitch(62),
            lost_actor: ANN.0,
            lost_user: ANN.1,
            lost_seq: 1,
        }]
    );
    assert_eq!(note(&seq).pitch, 64, "the later write wins");

    // Submitting the lost op again restores it; having seen Ben's edit, it displaces nothing.
    assert!(superseded(&mut seq, ANN, txn(2, 2, vec![lost[0].lost.clone()])).is_empty());
    assert_eq!(note(&seq).pitch, 62);
}

#[test]
fn only_the_newest_unseen_write_is_displaced() {
    let mut seq = Sequencer::new(base(), 0, vec![]);
    superseded(&mut seq, ANN, txn(1, 0, vec![pitch(61)]));
    superseded(&mut seq, ANN, txn(2, 1, vec![pitch(62)]));
    let lost = superseded(&mut seq, BEN, txn(1, 0, vec![pitch(70)]));
    assert_eq!(lost.len(), 1);
    assert_eq!((lost[0].lost.clone(), lost[0].lost_seq), (pitch(62), 2));
}

#[test]
fn nothing_is_superseded_without_a_real_conflict() {
    let mut seq = Sequencer::new(base(), 0, vec![]);
    superseded(&mut seq, ANN, txn(1, 0, vec![pitch(62)]));

    // Seen before editing.
    assert!(superseded(&mut seq, BEN, txn(1, 1, vec![pitch(63)])).is_empty());
    // Another property of the same note.
    assert!(superseded(&mut seq, ANN, txn(2, 1, vec![vel(80)])).is_empty());
    // One's own earlier write, made on another connection of the same user.
    assert!(superseded(&mut seq, (ActorId(7), BEN.1), txn(1, 0, vec![pitch(65)])).is_empty());
    // The same value.
    assert!(superseded(&mut seq, ANN, txn(3, 0, vec![pitch(65)])).is_empty());
}

#[test]
fn an_edit_inside_something_removed_unseen_is_itself_what_got_lost() {
    let mut seq = Sequencer::new(base(), 0, vec![]);
    superseded(
        &mut seq,
        ANN,
        txn(
            1,
            0,
            vec![Op::ClipRemove {
                track: TRACK,
                id: CLIP,
            }],
        ),
    );

    let added = Op::NoteAdd {
        track: TRACK,
        clip: CLIP,
        id: NoteId(EntityId(77)),
        note: Note {
            start: Tick::beats(2),
            dur: Tick::beats(1),
            pitch: 67,
            vel: 100,
        },
    };
    let lost = superseded(&mut seq, BEN, txn(1, 0, vec![added.clone(), pitch(72)]));
    assert_eq!(lost.len(), 2);
    assert_eq!(
        (lost[0].lost.clone(), lost[0].lost_actor, lost[0].lost_seq),
        (added, BEN.0, 2)
    );
    assert_eq!(lost[1].lost, pitch(72));

    // Editing something that was never there displaces nothing.
    let stray = Op::NoteSet {
        track: TRACK,
        clip: ClipId(EntityId(404)),
        id: NOTE,
        prop: NoteProp::Vel(1),
    };
    assert!(superseded(&mut seq, BEN, txn(2, 0, vec![stray])).is_empty());
}

#[test]
fn recent_events_can_be_read_back() {
    let mut seq = Sequencer::new(base(), 0, vec![]);
    for n in 1..=5 {
        seq.sequence(
            ANN.0,
            ANN.1,
            0,
            txn(n, n - 1, vec![pitch(60 + n as u8)]),
            has_blob,
        )
        .unwrap();
    }
    let seqs = |after| {
        seq.events_after(after)
            .map(|es| es.iter().map(|e| e.seq).collect::<Vec<_>>())
    };
    assert_eq!(seqs(0), Some(vec![1, 2, 3, 4, 5]));
    assert_eq!(seqs(3), Some(vec![4, 5]));
    assert_eq!(seqs(5), Some(vec![]));

    // Restarted with only the last two events at hand.
    let recent = seq.events_after(3).unwrap();
    let seq = Sequencer::new(seq.doc().clone(), 5, recent);
    assert_eq!(seq.events_after(3).map(|es| es.len()), Some(2));
    assert_eq!(
        seq.events_after(2),
        None,
        "older events have to come from the store"
    );
}
