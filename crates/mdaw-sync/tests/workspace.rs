//! Checkouts on the client: edits held in a journal, and what happens when the checkout ends.

use mdaw_model::*;
use mdaw_proto::{ChangelogEntry, Event, EventBody, ReleaseReason, SyncData};
use mdaw_sync::{Ended, Replica};

const ANN: UserId = UserId(1);
const BEN: UserId = UserId(2);
const TRACK: TrackId = TrackId(EntityId(1));
const CLIP: ClipId = ClipId(EntityId(2));
const TEMPO: TempoId = TempoId(EntityId(3));

fn base() -> Project {
    let mut project = Project::default();
    let ops = [
        Op::TempoPut {
            id: TEMPO,
            point: TempoPoint {
                tick: Tick::ZERO,
                milli_bpm: 120_000,
            },
        },
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
    ];
    for op in &ops {
        apply(&mut project, op);
    }
    project
}

fn add(n: u64) -> Op {
    Op::NoteAdd {
        track: TRACK,
        clip: CLIP,
        id: NoteId(EntityId(100 + n)),
        note: Note {
            start: Tick::beats(n as i64),
            dur: Tick::beats(1),
            pitch: 60,
            vel: 100,
        },
    }
}

fn notes(project: &Project) -> usize {
    project.track(TRACK).unwrap().content.clips[&CLIP].notes.len()
}

fn event(seq: u64, user: UserId, body: EventBody) -> Event {
    Event {
        seq,
        actor: ActorId(user.0),
        user,
        wall_ts: 0,
        body,
    }
}

fn ann() -> Replica {
    let mut replica = Replica::new(ActorId(1), base(), 0);
    replica.set_user(ANN);
    replica
}

#[test]
fn edits_of_a_checkout_stay_in_its_journal() {
    let mut replica = ann();
    replica
        .on_event(&event(1, ANN, EventBody::Checkout { track: TRACK }))
        .unwrap();
    assert!(replica.workspace(TRACK).is_some());

    let before = replica.revision();
    assert_eq!(
        replica.submit(vec![add(1), add(2)]),
        None,
        "nothing goes on the wire"
    );
    assert_eq!(replica.revision(), before + 1);
    assert_eq!(notes(replica.view()), 2, "the view has the work");
    assert_eq!(notes(replica.confirmed()), 0, "the shared document does not");
    assert_eq!(replica.workspace(TRACK).unwrap().journal.len(), 2);

    // Undo works inside the journal too.
    assert_eq!(replica.undo(), None);
    assert_eq!(notes(replica.view()), 0);
    assert_eq!(replica.workspace(TRACK).unwrap().journal.len(), 4);
    replica.redo();
    assert_eq!(notes(replica.view()), 2);

    // A project edit beside it still goes live.
    let rename = Op::ProjectSetName { name: "x".into() };
    assert!(replica.submit(vec![rename]).is_some());

    // Others' events keep the journal on top.
    let other = Op::TrackSetShell {
        id: TRACK,
        prop: ShellProp::Name("renamed".into()),
    };
    let txn = EventBody::Txn {
        client_txn_id: 1,
        ops: vec![other],
        superseded: vec![],
    };
    replica.on_event(&event(2, BEN, txn)).unwrap();
    assert_eq!(notes(replica.view()), 2);
    assert_eq!(replica.view().track(TRACK).unwrap().shell.name, "renamed");
}

#[test]
fn a_commit_moves_the_journal_into_the_document() {
    let mut replica = ann();
    replica
        .on_event(&event(1, ANN, EventBody::Checkout { track: TRACK }))
        .unwrap();
    replica.submit(vec![add(1)]);
    let workspace = replica.begin_commit(TRACK).unwrap();
    assert!(replica.begin_commit(TRACK).is_none(), "one commit at a time");
    assert_eq!(replica.submit(vec![add(2)]), None);
    assert_eq!(notes(replica.view()), 1, "no edits while committing");
    assert!(!replica.can_edit_content(TRACK));

    let commit = EventBody::Commit {
        track: TRACK,
        ops: workspace.journal,
        project_fix: vec![],
        stem: None,
        entry: ChangelogEntry::default(),
    };
    replica.on_event(&event(2, ANN, commit)).unwrap();
    assert!(replica.workspace(TRACK).is_none());
    assert_eq!(notes(replica.confirmed()), 1);
    assert_eq!(notes(replica.view()), 1);
    let ended = replica.take_ended();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].1, Ended::Committed);
    assert!(replica.can_edit_content(TRACK));
}

#[test]
fn a_cancelled_commit_takes_edits_again() {
    let mut replica = ann();
    replica
        .on_event(&event(1, ANN, EventBody::Checkout { track: TRACK }))
        .unwrap();
    replica.begin_commit(TRACK).unwrap();
    replica.cancel_commit(TRACK);
    replica.submit(vec![add(1)]);
    assert_eq!(notes(replica.view()), 1);
}

#[test]
fn a_forced_release_hands_the_journal_back() {
    let mut replica = ann();
    replica
        .on_event(&event(1, ANN, EventBody::Checkout { track: TRACK }))
        .unwrap();
    replica.submit(vec![add(1)]);
    let release = EventBody::Release {
        track: TRACK,
        reason: ReleaseReason::Forced { owner: ANN },
    };
    replica.on_event(&event(2, BEN, release)).unwrap();
    assert_eq!(notes(replica.view()), 0, "back to what everyone has");
    let ended = replica.take_ended();
    assert_eq!(ended[0].1, Ended::Forced { by: BEN });
    assert_eq!(ended[0].0.journal, [add(1)]);
}

#[test]
fn others_checkouts_make_no_workspace() {
    let mut replica = ann();
    replica
        .on_event(&event(1, BEN, EventBody::Checkout { track: TRACK }))
        .unwrap();
    assert!(replica.workspace(TRACK).is_none());
    assert!(!replica.can_edit_content(TRACK));
}

#[test]
fn a_snapshot_with_an_own_checkout_brings_a_workspace_to_fill() {
    let mut doc = base();
    doc.set_checkout(
        TRACK,
        Some(Checkout {
            user: ANN,
            base_seq: 5,
        }),
    );
    let mut replica = Replica::new(ActorId(9), base(), 0);
    replica.set_user(ANN);
    replica
        .resync(ActorId(9), 5, SyncData::Snapshot(doc), None)
        .unwrap();
    let workspace = replica.workspace(TRACK).unwrap();
    assert!(workspace.journal.is_empty());

    // The journal saved by an earlier run comes back.
    assert!(replica.restore_journal(TRACK, vec![add(3)], workspace.base_tempo.clone()));
    assert_eq!(notes(replica.view()), 1);
}

#[test]
fn the_tempo_at_checkout_is_remembered() {
    let mut replica = ann();
    replica
        .on_event(&event(1, ANN, EventBody::Checkout { track: TRACK }))
        .unwrap();
    let faster = EventBody::Txn {
        client_txn_id: 1,
        ops: vec![Op::TempoPut {
            id: TEMPO,
            point: TempoPoint {
                tick: Tick::ZERO,
                milli_bpm: 140_000,
            },
        }],
        superseded: vec![],
    };
    replica.on_event(&event(2, BEN, faster)).unwrap();
    let workspace = replica.workspace(TRACK).unwrap();
    assert_eq!(workspace.base_tempo[&TEMPO].milli_bpm, 120_000);
    assert_eq!(replica.confirmed().tempo[&TEMPO].milli_bpm, 140_000);
}
