//! Checkout, commit, release and blobs at the hub, without a network.

use mdaw_model::*;
use mdaw_proto::*;
use mdaw_server::{Blobs, Hub, Sink, Store};
use std::collections::VecDeque;
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

    fn events(&self) -> Vec<Event> {
        self.drain()
            .into_iter()
            .filter_map(|m| match m {
                ServerMsg::Event(e) => Some(e),
                _ => None,
            })
            .collect()
    }

    fn denied(&self) -> Vec<(Request, RejectReason)> {
        self.drain()
            .into_iter()
            .filter_map(|m| match m {
                ServerMsg::Denied { request, reason } => Some((request, reason)),
                _ => None,
            })
            .collect()
    }
}

struct Client {
    actor: ActorId,
    user: UserId,
    inbox: Inbox,
}

fn join(hub: &mut Hub, key: u8) -> Client {
    let inbox = Inbox::default();
    let hello = Hello {
        proto_version: PROTO_VERSION,
        token: hub.token().to_string(),
        user_key: [key; 16],
        name: format!("user {key}"),
        resume: None,
    };
    let actor = hub.join(hello, inbox.sink()).unwrap();
    let ServerMsg::Welcome(welcome) = inbox.drain().remove(0) else {
        panic!("expected a welcome")
    };
    Client {
        actor,
        user: welcome.user,
        inbox,
    }
}

const TRACK: TrackId = TrackId(EntityId(1 << 32 | 1));
const CLIP: ClipId = ClipId(EntityId(1 << 32 | 2));

/// A hub with one track holding one empty clip, and two clients.
fn setup(store: Store) -> (Hub, Client, Client) {
    let mut hub = Hub::open(store, Blobs::in_memory()).unwrap();
    let ann = join(&mut hub, 1);
    let ben = join(&mut hub, 2);
    let clip = Clip {
        start: Tick::ZERO,
        length: Tick::bars(2),
        name: "c".into(),
        notes: Default::default(),
    };
    let txn = Txn {
        client_txn_id: 1,
        base_seq: 0,
        ops: vec![
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
                clip,
            },
        ],
    };
    hub.submit(ann.actor, txn);
    ann.inbox.drain();
    ben.inbox.drain();
    (hub, ann, ben)
}

fn note(n: u32) -> Op {
    Op::NoteAdd {
        track: TRACK,
        clip: CLIP,
        id: NoteId(EntityId(1 << 32 | (100 + n as u64))),
        note: Note {
            start: Tick::beats(n as i64),
            dur: Tick::beats(1),
            pitch: 60,
            vel: 100,
        },
    }
}

fn sample(blob: BlobHash) -> Sample {
    Sample {
        kind: SampleKind::Solo,
        blob,
        range: TickRange::new(Tick::ZERO, Tick::bars(1)),
        ms: 2000,
    }
}

fn commit(ops: Vec<Op>, samples: Vec<Sample>) -> ClientMsg {
    ClientMsg::Commit(Box::new(CommitRequest {
        track: TRACK,
        ops,
        project_fix: Vec::new(),
        stem: None,
        entry: ChangelogEntry {
            summary: "two notes".into(),
            ranges: vec![TickRange::new(Tick::ZERO, Tick::bars(1))],
            samples,
        },
    }))
}

#[test]
fn a_checkout_locks_the_track_for_everyone_else_until_the_commit() {
    let (mut hub, ann, ben) = setup(Store::in_memory().unwrap());
    hub.handle(ann.actor, ClientMsg::Checkout { track: TRACK });
    let [event] = &ann.inbox.events()[..] else {
        panic!()
    };
    assert_eq!(event.body, EventBody::Checkout { track: TRACK });
    assert_eq!(ben.inbox.events().len(), 1, "everyone hears of it");
    let checkout = hub.doc().track(TRACK).unwrap().checkout.unwrap();
    assert_eq!((checkout.user, checkout.base_seq), (ann.user, 2));

    // Ben can neither edit the contents nor take the track himself.
    let live = Txn {
        client_txn_id: 1,
        base_seq: 2,
        ops: vec![note(9)],
    };
    hub.submit(ben.actor, live);
    assert!(matches!(
        &ben.inbox.drain()[..],
        [ServerMsg::Rejected { reason: RejectReason::CheckedOut { by, .. }, .. }] if *by == ann.user
    ));
    hub.handle(ben.actor, ClientMsg::Checkout { track: TRACK });
    assert!(matches!(
        &ben.inbox.denied()[..],
        [(Request::Checkout(TRACK), RejectReason::CheckedOut { .. })]
    ));
    // Nor commit in her place.
    hub.handle(ben.actor, commit(vec![note(1)], vec![]));
    assert_eq!(
        ben.inbox.denied(),
        [(Request::Commit(TRACK), RejectReason::NotOwner { track: TRACK })]
    );

    // A commit that names audio the server lacks is turned down.
    let audio = b"pretend this is FLAC".to_vec();
    let blob = blob_hash(&audio);
    hub.handle(ann.actor, commit(vec![note(1), note(2)], vec![sample(blob)]));
    assert_eq!(
        ann.inbox.denied(),
        [(Request::Commit(TRACK), RejectReason::MissingBlob(blob))]
    );
    // So is one that reaches into the project.
    hub.handle(
        ann.actor,
        commit(vec![Op::ProjectSetName { name: "x".into() }], vec![]),
    );
    assert_eq!(
        ann.inbox.denied(),
        [(Request::Commit(TRACK), RejectReason::OutOfScope)]
    );

    assert!(hub.blobs().put(&blob, &audio).unwrap());
    hub.handle(ann.actor, commit(vec![note(1), note(2)], vec![sample(blob)]));
    let [event] = &ben.inbox.events()[..] else {
        panic!()
    };
    let EventBody::Commit { entry, ops, .. } = &event.body else {
        panic!()
    };
    assert_eq!((ops.len(), entry.samples[0].blob), (2, blob));
    assert_eq!(event.user, ann.user);
    let track = hub.doc().track(TRACK).unwrap();
    assert_eq!(track.checkout, None, "live again");
    assert_eq!(track.content.clips[&CLIP].notes.len(), 2);

    // Live edits are taken again.
    let live = Txn {
        client_txn_id: 2,
        base_seq: hub.head(),
        ops: vec![note(9)],
    };
    hub.submit(ben.actor, live);
    assert_eq!(ben.inbox.events().len(), 1);
}

#[test]
fn a_checkout_ends_by_its_owner_or_by_force() {
    let (mut hub, ann, ben) = setup(Store::in_memory().unwrap());
    hub.handle(ben.actor, ClientMsg::Release { track: TRACK });
    assert_eq!(
        ben.inbox.denied(),
        [(
            Request::Release(TRACK),
            RejectReason::NotCheckedOut { track: TRACK }
        )]
    );

    hub.handle(ann.actor, ClientMsg::Checkout { track: TRACK });
    hub.handle(ben.actor, ClientMsg::Release { track: TRACK });
    assert_eq!(
        ben.inbox.denied(),
        [(Request::Release(TRACK), RejectReason::NotOwner { track: TRACK })]
    );
    hub.handle(ben.actor, ClientMsg::ForceRelease { track: TRACK });
    let event = ann.inbox.events().pop().unwrap();
    assert_eq!(event.user, ben.user);
    assert_eq!(
        event.body,
        EventBody::Release {
            track: TRACK,
            reason: ReleaseReason::Forced { owner: ann.user }
        }
    );
    assert_eq!(hub.doc().track(TRACK).unwrap().checkout, None);

    hub.handle(ann.actor, ClientMsg::Checkout { track: TRACK });
    hub.handle(ann.actor, ClientMsg::Release { track: TRACK });
    let event = ben.inbox.events().pop().unwrap();
    assert_eq!(
        event.body,
        EventBody::Release {
            track: TRACK,
            reason: ReleaseReason::Abandoned
        }
    );
}

#[test]
fn only_the_user_whose_edit_lost_attaches_how_it_sounded() {
    let (mut hub, ann, ben) = setup(Store::in_memory().unwrap());
    // Both rename the clip from seq 1; Ann's comes second and displaces Ben's.
    let rename = |name: &str| Txn {
        client_txn_id: 2,
        base_seq: 1,
        ops: vec![Op::ClipSet {
            track: TRACK,
            id: CLIP,
            prop: ClipProp::Name(name.into()),
        }],
    };
    hub.submit(ben.actor, rename("ben's"));
    hub.submit(ann.actor, rename("ann's"));
    let entry = ann.inbox.events().pop().unwrap();
    let EventBody::Txn { superseded, .. } = &entry.body else {
        panic!("expected a txn")
    };
    assert_eq!(superseded[0].lost_user, ben.user);
    ben.inbox.drain();

    let audio = vec![7u8; 100];
    let blob = blob_hash(&audio);
    let before = Sample {
        kind: SampleKind::Before,
        ..sample(blob)
    };
    let attach = |hub: &mut Hub, who: &Client, entry_seq: u64, sample: Sample| {
        hub.handle(who.actor, ClientMsg::Attach { entry_seq, sample });
        who.inbox.denied()
    };
    let refused = |seq| [(Request::Attach(seq), RejectReason::NoSuchEntry)];
    assert_eq!(attach(&mut hub, &ben, 99, before), refused(99));
    assert_eq!(
        attach(&mut hub, &ben, 1, before),
        refused(1),
        "nothing of Ben's lost there"
    );
    assert_eq!(
        attach(&mut hub, &ann, entry.seq, before),
        refused(entry.seq),
        "not Ann's"
    );
    assert_eq!(
        attach(&mut hub, &ben, entry.seq, sample(blob)),
        refused(entry.seq),
        "only how it sounded before"
    );
    assert_eq!(
        attach(&mut hub, &ben, entry.seq, before),
        [(Request::Attach(entry.seq), RejectReason::MissingBlob(blob))]
    );

    hub.blobs().put(&blob, &audio).unwrap();
    assert!(attach(&mut hub, &ben, entry.seq, before).is_empty());
    let event = ann.inbox.events().pop().unwrap();
    assert_eq!(
        event.body,
        EventBody::Attach {
            entry_seq: entry.seq,
            sample: before
        }
    );
    assert_eq!(
        attach(&mut hub, &ben, entry.seq, before),
        refused(entry.seq),
        "one per track"
    );
}

#[test]
fn blobs_are_stored_only_under_their_own_hash() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = Blobs::open(dir.path().join("blobs")).unwrap();
    let bytes = b"some audio".to_vec();
    let hash = blob_hash(&bytes);
    assert!(!blobs.has(&hash));
    assert!(!blobs.put(&blob_hash(b"other"), &bytes).unwrap());
    assert!(blobs.put(&hash, &bytes).unwrap());
    assert!(blobs.has(&hash));
    let again = Blobs::open(dir.path().join("blobs")).unwrap();
    assert_eq!(again.get(&hash).unwrap(), Some(bytes));
    assert_eq!(again.get(&blob_hash(b"other")).unwrap(), None);
}

#[test]
fn commits_survive_a_restart_and_new_clients_see_them_in_the_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("project.db");
    let (mut hub, ann, _ben) = setup(Store::open(&path).unwrap());
    hub.handle(ann.actor, ClientMsg::Checkout { track: TRACK });
    hub.handle(ann.actor, commit(vec![note(1)], vec![]));
    hub.handle(ann.actor, ClientMsg::Checkout { track: TRACK });
    let doc = hub.doc().clone();
    drop(hub);

    let mut hub = Hub::open(Store::open(&path).unwrap(), Blobs::in_memory()).unwrap();
    assert_eq!(hub.doc(), &doc);
    assert!(
        hub.doc().track(TRACK).unwrap().checkout.is_some(),
        "still checked out"
    );
    let inbox = Inbox::default();
    let hello = Hello {
        proto_version: PROTO_VERSION,
        token: hub.token().to_string(),
        user_key: [3; 16],
        name: "cid".into(),
        resume: None,
    };
    hub.join(hello, inbox.sink()).unwrap();
    let ServerMsg::Welcome(welcome) = inbox.drain().remove(0) else {
        panic!()
    };
    let kinds: Vec<_> = welcome
        .history
        .iter()
        .map(|e| std::mem::discriminant(&e.body))
        .collect();
    assert_eq!(
        kinds.len(),
        3,
        "checkout, commit, checkout; the plain edit is not news"
    );
    assert!(matches!(welcome.history[1].body, EventBody::Commit { .. }));
}

#[test]
fn only_the_owner_renews_a_lease_and_nothing_is_stale_at_first() {
    let (mut hub, ann, ben) = setup(Store::in_memory().unwrap());
    hub.handle(ann.actor, ClientMsg::Checkout { track: TRACK });
    hub.handle(ann.actor, ClientMsg::LeaseRenew { track: TRACK });
    hub.handle(ben.actor, ClientMsg::LeaseRenew { track: TRACK });
    hub.tick();
    assert!(hub.stale().is_empty());
    assert!(
        !ben.inbox
            .drain()
            .iter()
            .any(|m| matches!(m, ServerMsg::Stale(_) | ServerMsg::Denied { .. }))
    );
}
