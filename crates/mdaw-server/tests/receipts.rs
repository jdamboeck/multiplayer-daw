//! Exactly once: a transaction sent again, after a lost acknowledgement, a reconnect, or a restart
//! of the server, is answered as the first time and applied never again. And durably: an event
//! is on disk, with its receipt, before anyone hears of it.

use mdaw_model::*;
use mdaw_proto::*;
use mdaw_server::{Blobs, Hub, Receipt, Sink, Store};
use mdaw_sync::Replica;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Inbox(Arc<Mutex<VecDeque<ServerMsg>>>);

impl Inbox {
    fn sink(&self) -> Sink {
        let queue = self.0.clone();
        Box::new(move |msg| {
            queue.lock().unwrap().push_back(msg);
            true
        })
    }

    fn drain(&self) -> Vec<ServerMsg> {
        self.0.lock().unwrap().drain(..).collect()
    }

    /// Everything but the list of peers.
    fn edits(&self) -> Vec<ServerMsg> {
        self.drain()
            .into_iter()
            .filter(|m| !matches!(m, ServerMsg::Peers(_)))
            .collect()
    }
}

struct Client {
    actor: ActorId,
    inbox: Inbox,
    welcome: Welcome,
}

fn connect(hub: &mut Hub, key: u8, resume: Option<Resume>) -> Client {
    let inbox = Inbox::default();
    let hello = Hello {
        proto_version: PROTO_VERSION,
        token: hub.token().to_string(),
        user_key: [key; 16],
        name: format!("user {key}"),
        resume,
    };
    let actor = hub.join(hello, inbox.sink()).unwrap();
    let ServerMsg::Welcome(welcome) = inbox.drain().remove(0) else {
        panic!("expected a welcome")
    };
    Client {
        actor,
        inbox,
        welcome: *welcome,
    }
}

fn resume_of(client: &Client, last_seq: u64) -> Option<Resume> {
    Some(Resume {
        prev_actor: client.actor,
        session_secret: client.welcome.session_secret,
        last_seq,
    })
}

const TRACK: TrackId = TrackId(EntityId(1 << 32 | 1));
const CLIP: ClipId = ClipId(EntityId(1 << 32 | 2));
const NOTE: NoteId = NoteId(EntityId(1 << 32 | 3));
/// The note Ann adds in the transaction that loses its acknowledgement.
const ANNS: NoteId = NoteId(EntityId(1 << 32 | 4));
/// A note Ben adds while Ann is away.
const BENS: NoteId = NoteId(EntityId(2 << 32 | 1));

fn a_note(pitch: u8) -> Note {
    Note {
        start: Tick::ZERO,
        dur: Tick::beats(1),
        pitch,
        vel: 100,
    }
}

fn add_note(id: NoteId) -> Op {
    Op::NoteAdd {
        track: TRACK,
        clip: CLIP,
        id,
        note: a_note(60),
    }
}

fn set_pitch(pitch: u8) -> Op {
    Op::NoteSet {
        track: TRACK,
        clip: CLIP,
        id: NOTE,
        prop: NoteProp::Pitch(pitch),
    }
}

fn txn(base_seq: u64, ops: Vec<Op>) -> Txn {
    Txn {
        id: TxnId::random(),
        base_seq,
        ops,
    }
}

/// A track with a clip holding one note, made by `who`: event 1.
fn seed(hub: &mut Hub, who: &Client) {
    let ops = vec![
        Op::TrackAdd {
            id: TRACK,
            shell: TrackShell {
                name: "t".into(),
                color: 0,
            },
            content: TrackContent::default(),
            after: None,
        },
        Op::ClipAdd {
            track: TRACK,
            id: CLIP,
            clip: Clip {
                start: Tick::ZERO,
                length: Tick::bars(1),
                name: String::new(),
                notes: Default::default(),
            },
        },
        Op::NoteAdd {
            track: TRACK,
            clip: CLIP,
            id: NOTE,
            note: a_note(60),
        },
    ];
    hub.submit(who.actor, txn(0, ops));
    assert_eq!(hub.head(), 1);
}

fn clip_notes(project: &Project) -> Vec<(NoteId, u8)> {
    let clip = &project.track(TRACK).unwrap().content.clips[&CLIP];
    clip.notes.iter().map(|(id, n)| (*id, n.pitch)).collect()
}

/// Every event on disk, read through a connection of its own.
fn stored_events(path: &Path) -> Vec<Event> {
    Store::open(path).unwrap().events_after(0, u64::MAX >> 1).unwrap()
}

fn plugin(state: BlobHash) -> Op {
    Op::DeviceAdd {
        track: TRACK,
        device: Device::new(
            DeviceId(EntityId(1 << 32 | 9)),
            DeviceKind::Plugin {
                format: PluginFormat::Clap,
                id: "x".into(),
                name: "Synth".into(),
                state: Some(state),
            },
        ),
        after: None,
    }
}

// 1
#[test]
fn a_transaction_sent_twice_on_one_connection_is_one_event() {
    let mut hub = Hub::open(Store::in_memory().unwrap(), Blobs::in_memory()).unwrap();
    let ann = connect(&mut hub, 1, None);
    let ben = connect(&mut hub, 2, None);
    seed(&mut hub, &ann);
    ann.inbox.drain();
    ben.inbox.drain();

    let t = txn(1, vec![set_pitch(70)]);
    hub.submit(ann.actor, t.clone());
    hub.submit(ann.actor, t.clone());
    let answers = ann.inbox.edits();
    let [ServerMsg::Event(event), ServerMsg::Accepted { txn, seq }] = &answers[..] else {
        panic!("one event, then the receipt: {answers:?}")
    };
    assert_eq!((event.seq, event.txn), (2, t.id));
    assert_eq!((*txn, *seq), (t.id, 2), "the same seq as the first time");
    let others = ben.inbox.edits();
    assert!(
        matches!(&others[..], [ServerMsg::Event(e)] if e.seq == 2),
        "the others hear of it once: {others:?}"
    );
    assert_eq!(hub.head(), 2);
}

/// Ann sets the pitch of Ben's note and adds a note of her own; the server takes it, and the
/// connection breaks before she reads the event. Meanwhile Ben removes her note and sets the
/// pitch again. Done twice, her edit would bring the note back and undo Ben's pitch.
fn lost_ack(catch_up_limit: u64) -> (Hub, Replica, Client, Txn) {
    let hub = Hub::open(Store::in_memory().unwrap(), Blobs::in_memory()).unwrap();
    let mut hub = hub.with_catch_up_limit(catch_up_limit);
    let ann = connect(&mut hub, 1, None);
    let ben = connect(&mut hub, 2, None);
    seed(&mut hub, &ben);
    let SyncData::Snapshot(start) = &ann.welcome.sync else {
        panic!()
    };
    let mut replica = Replica::new(ann.actor, start.clone(), 0);
    replica.on_event(&stored_event(&ann.inbox, 1)).unwrap();

    let t = replica.submit(vec![add_note(ANNS), set_pitch(62)]).unwrap();
    hub.submit(ann.actor, t.clone());
    assert_eq!(hub.head(), 2, "taken");
    // Cut: nothing that was sent to Ann after this reaches her.
    hub.leave(ann.actor);
    ann.inbox.drain();

    let removed = Op::NoteRemove {
        track: TRACK,
        clip: CLIP,
        id: ANNS,
    };
    hub.submit(ben.actor, txn(2, vec![removed, set_pitch(65)]));
    hub.submit(ben.actor, txn(3, vec![add_note(BENS)]));
    assert_eq!(hub.head(), 4);
    (hub, replica, ann, t)
}

/// The next event in an inbox, skipping the rest.
fn stored_event(inbox: &Inbox, seq: u64) -> Event {
    inbox
        .drain()
        .into_iter()
        .find_map(|m| match m {
            ServerMsg::Event(e) if e.seq == seq => Some(e),
            _ => None,
        })
        .expect("the event")
}

fn after_ben(project: &Project) {
    let mut notes = clip_notes(project);
    notes.sort();
    assert_eq!(
        notes,
        [(NOTE, 65), (BENS, 60)],
        "Ann's note stays removed, Ben's pitch stays"
    );
}

/// Ann comes back, takes in what the server sends, and sends again everything still pending.
fn come_back(hub: &mut Hub, replica: &mut Replica, ann: &Client) -> (Client, Vec<Txn>) {
    let back = connect(hub, 1, resume_of(ann, replica.seq()));
    let resend = replica
        .resync(back.actor, back.welcome.head_seq, back.welcome.sync.clone())
        .unwrap();
    (back, resend)
}

fn deliver(replica: &mut Replica, inbox: &Inbox) {
    for msg in inbox.drain() {
        match msg {
            ServerMsg::Event(e) => replica.on_event(&e).unwrap(),
            ServerMsg::Accepted { txn, seq } => replica.on_accepted(txn, seq),
            ServerMsg::Rejected { txn, .. } => {
                replica.on_rejected(txn);
            }
            _ => {}
        }
    }
}

// 2
#[test]
fn a_lost_acknowledgement_and_a_catch_up_apply_the_edit_once() {
    let (mut hub, mut replica, ann, t) = lost_ack(5000);
    let (back, resend) = come_back(&mut hub, &mut replica, &ann);
    assert!(matches!(back.welcome.sync, SyncData::Events(ref e) if e.len() == 3));
    assert!(resend.is_empty(), "the catch-up held Ann's event");
    // A client that sends it again anyway gets the original answer.
    hub.submit(back.actor, t.clone());
    let answers = back.inbox.edits();
    assert_eq!(answers, [ServerMsg::Accepted { txn: t.id, seq: 2 }]);
    deliver(&mut replica, &back.inbox);
    assert_eq!(hub.head(), 4, "nothing was applied twice");
    after_ben(hub.doc());
    assert_eq!(replica.view(), hub.doc());
    assert_eq!(replica.pending().len(), 0);
}

// 3
#[test]
fn a_lost_acknowledgement_and_a_snapshot_apply_the_edit_once() {
    // Ann is two events behind; with a catch-up limit of one she gets the whole document.
    let (mut hub, mut replica, ann, t) = lost_ack(1);
    let (back, resend) = come_back(&mut hub, &mut replica, &ann);
    assert!(matches!(back.welcome.sync, SyncData::Snapshot(_)));
    assert_eq!(resend, std::slice::from_ref(&t), "still pending, same identity");
    // Until the answer, Ann's edit is on top of the snapshot a second time.
    assert!(clip_notes(replica.view()).contains(&(ANNS, 60)));
    for t in resend {
        hub.submit(back.actor, t);
    }
    let answers = back.inbox.edits();
    assert_eq!(answers, [ServerMsg::Accepted { txn: t.id, seq: 2 }]);
    for msg in answers {
        let ServerMsg::Accepted { txn, seq } = msg else {
            unreachable!()
        };
        assert!(seq <= replica.seq(), "in the snapshot");
        replica.on_accepted(txn, seq);
    }
    assert_eq!(replica.pending().len(), 0, "the receipt confirmed it");
    assert_eq!(hub.head(), 4);
    after_ben(hub.doc());
    assert_eq!(replica.view(), hub.doc());
}

// 4
#[test]
fn a_receipt_outlives_a_restart_of_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("project.db");
    let open = || Hub::open(Store::open(&path).unwrap(), Blobs::in_memory()).unwrap();
    let mut hub = open();
    let ann = connect(&mut hub, 1, None);
    seed(&mut hub, &ann);
    let t = txn(1, vec![set_pitch(70), add_note(ANNS)]);
    hub.submit(ann.actor, t.clone());
    let (doc, log) = (hub.doc().clone(), stored_events(&path));
    assert_eq!(log.len(), 2);
    drop(hub);

    let mut hub = open();
    let ann = connect(&mut hub, 1, resume_of(&ann, 1));
    hub.submit(ann.actor, t.clone());
    assert_eq!(ann.inbox.edits(), [ServerMsg::Accepted { txn: t.id, seq: 2 }]);
    assert_eq!(hub.head(), 2);
    assert_eq!(hub.doc(), &doc);
    assert_eq!(stored_events(&path), log, "the log is as it was");
}

// 5
#[test]
fn the_same_identity_from_two_users_is_two_transactions() {
    let mut hub = Hub::open(Store::in_memory().unwrap(), Blobs::in_memory()).unwrap();
    let ann = connect(&mut hub, 1, None);
    let ben = connect(&mut hub, 2, None);
    seed(&mut hub, &ann);
    let id = TxnId::from(42);
    let of = |ops| Txn { id, base_seq: 1, ops };
    hub.submit(ann.actor, of(vec![set_pitch(70)]));
    hub.submit(ben.actor, of(vec![add_note(BENS)]));
    assert_eq!(hub.head(), 3, "both taken");
    let mut notes = clip_notes(hub.doc());
    notes.sort();
    assert_eq!(notes, [(NOTE, 70), (BENS, 60)]);
    // Each user's receipt answers that user only.
    hub.submit(ben.actor, of(vec![set_pitch(1)]));
    assert!(
        ben.inbox
            .edits()
            .contains(&ServerMsg::Accepted { txn: id, seq: 3 })
    );
    hub.submit(ann.actor, of(vec![set_pitch(1)]));
    assert!(
        ann.inbox
            .edits()
            .contains(&ServerMsg::Accepted { txn: id, seq: 2 })
    );
}

// 6
#[test]
fn a_refused_transaction_stays_refused_and_a_new_identity_goes_through() {
    let mut hub = Hub::open(Store::in_memory().unwrap(), Blobs::in_memory()).unwrap();
    let ann = connect(&mut hub, 1, None);
    seed(&mut hub, &ann);
    ann.inbox.drain();
    let bytes = b"the plugin's settings";
    let state = blob_hash(bytes);
    let t = txn(1, vec![plugin(state)]);
    let refusal = ServerMsg::Rejected {
        txn: t.id,
        reason: RejectReason::MissingBlob(state),
    };
    hub.submit(ann.actor, t.clone());
    assert_eq!(ann.inbox.edits(), std::slice::from_ref(&refusal));

    // The blob is there now, but the transaction was refused, and that is final.
    assert!(hub.blobs().put(&state, bytes).unwrap());
    hub.submit(ann.actor, t.clone());
    assert_eq!(ann.inbox.edits(), [refusal]);
    assert_eq!(hub.head(), 1);

    // The same edit under a new identity is a new transaction.
    let again = Txn {
        id: TxnId::random(),
        ..t
    };
    hub.submit(ann.actor, again);
    assert!(matches!(&ann.inbox.edits()[..], [ServerMsg::Event(e)] if e.seq == 2));
}

// 7
#[test]
fn an_event_whose_receipt_cannot_be_written_is_neither_kept_nor_applied_nor_told() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("project.db");
    let mut hub = Hub::open(Store::open(&path).unwrap(), Blobs::in_memory()).unwrap();
    let ann = connect(&mut hub, 1, None);
    let ben = connect(&mut hub, 2, None);
    seed(&mut hub, &ann);
    ann.inbox.drain();
    ben.inbox.drain();
    let doc = hub.doc().clone();

    // The disk takes the event row, then fails on the receipt, inside the same transaction.
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "CREATE TRIGGER no_receipts BEFORE INSERT ON receipts BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .unwrap();
    let t = txn(1, vec![set_pitch(70)]);
    hub.submit(ann.actor, t.clone());

    let count = |table: &str| -> i64 {
        db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(count("events"), 1, "the event row went with the failed receipt");
    assert_eq!(count("receipts"), 1, "the seed's only");
    assert_eq!(hub.head(), 1, "not applied in memory");
    assert_eq!(hub.doc(), &doc);
    assert!(ann.inbox.edits().is_empty(), "the sender is told nothing");
    assert!(ben.inbox.edits().is_empty(), "nor is anyone else");
    assert!(
        hub.peers().iter().all(|p| p.actor != ann.actor),
        "the sender was dropped"
    );

    // Once the disk takes it, the transaction sent again lands, right after the last event.
    db.execute_batch("DROP TRIGGER no_receipts;").unwrap();
    let ann = connect(&mut hub, 1, resume_of(&ann, 1));
    hub.submit(ann.actor, t.clone());
    assert!(matches!(&ann.inbox.edits()[..], [ServerMsg::Event(e)] if e.seq == 2 && e.txn == t.id));
    assert_eq!(stored_events(&path).len(), 2);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.receipt(ann.welcome.user, t.id).unwrap(),
        Some(Receipt::Accepted(2))
    );
}

// 8
#[test]
fn events_are_committed_with_synchronous_full() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("project.db")).unwrap();
    assert_eq!(store.synchronous().unwrap(), 2, "FULL");
    // And it stays so after the writes that once switched it back and forth.
    let mut hub = Hub::open(store, Blobs::in_memory()).unwrap();
    let ann = connect(&mut hub, 1, None);
    seed(&mut hub, &ann);
    drop(hub);
    let store = Store::open(&dir.path().join("project.db")).unwrap();
    assert_eq!(store.synchronous().unwrap(), 2);
}

#[test]
fn a_refusal_is_kept_across_a_restart_too() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("project.db");
    let open = || Hub::open(Store::open(&path).unwrap(), Blobs::in_memory()).unwrap();
    let mut hub = open();
    let ann = connect(&mut hub, 1, None);
    seed(&mut hub, &ann);
    let t = txn(1, vec![plugin(blob_hash(b"never"))]);
    hub.submit(ann.actor, t.clone());
    drop(hub);
    let mut hub = open();
    let ann = connect(&mut hub, 1, None);
    hub.submit(ann.actor, t.clone());
    assert!(matches!(
        &ann.inbox.edits()[..],
        [ServerMsg::Rejected { txn, reason: RejectReason::MissingBlob(_) }] if *txn == t.id
    ));
    assert_eq!(hub.head(), 1);
}
