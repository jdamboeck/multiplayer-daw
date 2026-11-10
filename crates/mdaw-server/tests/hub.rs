//! The project hub without a network: clients are queues.

use mdaw_model::*;
use mdaw_proto::*;
use mdaw_server::{Hub, Sink, Store};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// What the server has sent to one connection.
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

    fn next(&self) -> Option<ServerMsg> {
        self.0.lock().unwrap().pop_front()
    }

    fn drain(&self) -> Vec<ServerMsg> {
        self.0.lock().unwrap().drain(..).collect()
    }

    fn welcome(&self) -> Welcome {
        match self.next() {
            Some(ServerMsg::Welcome(welcome)) => *welcome,
            other => panic!("expected a welcome, got {other:?}"),
        }
    }
}

fn hello(hub: &Hub, key: u8, resume: Option<Resume>) -> Hello {
    Hello {
        proto_version: PROTO_VERSION,
        token: hub.token().to_string(),
        user_key: [key; 16],
        name: format!("user {key}"),
        resume,
    }
}

fn join(hub: &mut Hub, key: u8) -> (ActorId, Inbox, Welcome) {
    rejoin(hub, key, None)
}

fn rejoin(hub: &mut Hub, key: u8, resume: Option<Resume>) -> (ActorId, Inbox, Welcome) {
    let inbox = Inbox::default();
    let actor = hub.join(hello(hub, key, resume), inbox.sink()).unwrap();
    let welcome = inbox.welcome();
    assert_eq!(welcome.actor, actor);
    (actor, inbox, welcome)
}

fn rename(n: u64) -> Txn {
    Txn {
        client_txn_id: n,
        base_seq: 0,
        ops: vec![Op::ProjectSetName {
            name: format!("take {n}"),
        }],
    }
}

fn clap(n: u64) -> Txn {
    let device = Device::new(
        DeviceId(EntityId(9)),
        DeviceKind::Clap {
            plugin_id: "x".into(),
            state: None,
        },
    );
    Txn {
        client_txn_id: n,
        base_seq: 0,
        ops: vec![Op::DeviceAdd {
            track: TrackId(EntityId(1)),
            device,
            after: None,
        }],
    }
}

fn hub() -> Hub {
    Hub::open(Store::in_memory().unwrap()).unwrap()
}

#[test]
fn a_new_project_has_a_tempo_and_no_tracks() {
    let hub = hub();
    assert_eq!(hub.head(), 0);
    assert_eq!(hub.doc().tracks.len(), 0);
    assert_eq!(hub.doc().tempo_map().milli_bpm_at(Tick::ZERO), DEFAULT_MILLI_BPM);
    assert_eq!(hub.doc().tempo.len(), 1);
    assert_eq!(hub.token().len(), 32);
}

#[test]
fn joining_needs_the_token_and_the_same_protocol() {
    let mut hub = hub();
    let mut wrong = hello(&hub, 1, None);
    wrong.token.push('x');
    assert_eq!(
        hub.join(wrong, Inbox::default().sink()),
        Err(RefuseReason::BadToken)
    );

    let mut old = hello(&hub, 1, None);
    old.proto_version += 1;
    assert_eq!(
        hub.join(old, Inbox::default().sink()),
        Err(RefuseReason::ProtoVersion(PROTO_VERSION))
    );
    assert!(hub.peers().is_empty());
}

#[test]
fn a_welcome_carries_the_document_and_everyone_learns_who_is_there() {
    let mut hub = hub();
    let (ann, ann_inbox, welcome) = join(&mut hub, 1);
    assert_eq!(welcome.head_seq, 0);
    assert_eq!(welcome.sync, SyncData::Snapshot(hub.doc().clone()));
    assert_eq!(welcome.acked, None);
    assert_eq!(
        welcome.peers,
        [Peer {
            actor: ann,
            user: welcome.user,
            name: "user 1".into()
        }]
    );
    assert_eq!(ann_inbox.drain(), [ServerMsg::Peers(welcome.peers.clone())]);

    let (ben, ben_inbox, ben_welcome) = join(&mut hub, 2);
    assert_ne!(ann, ben);
    assert_ne!(welcome.user, ben_welcome.user);
    assert_eq!(ben_welcome.peers.len(), 2);
    assert_eq!(ann_inbox.drain(), [ServerMsg::Peers(ben_welcome.peers.clone())]);

    // The same install on a second connection is the same user under another actor.
    let (again, _, again_welcome) = join(&mut hub, 1);
    assert_ne!(again, ann);
    assert_eq!(again_welcome.user, welcome.user);

    ben_inbox.drain();
    hub.leave(ben);
    assert_eq!(hub.peers().len(), 2);
    assert!(ben_inbox.drain().is_empty(), "whoever left hears nothing more");
    assert!(matches!(ann_inbox.drain().last(), Some(ServerMsg::Peers(p)) if p.len() == 2));
}

#[test]
fn events_go_to_everyone_and_rejections_to_the_sender() {
    let mut hub = hub();
    let (ann, ann_inbox, _) = join(&mut hub, 1);
    let (_ben, ben_inbox, _) = join(&mut hub, 2);
    ann_inbox.drain();
    ben_inbox.drain();

    hub.submit(ann, rename(1));
    let [ServerMsg::Event(event)] = &ann_inbox.drain()[..] else {
        panic!("expected one event")
    };
    assert_eq!((event.seq, event.actor, event.client_txn_id()), (1, ann, Some(1)));
    assert_eq!(ben_inbox.drain(), [ServerMsg::Event(event.clone())]);
    assert_eq!(hub.doc().name, "take 1");

    hub.submit(ann, clap(2));
    assert_eq!(
        ann_inbox.drain(),
        [ServerMsg::Rejected {
            client_txn_id: 2,
            reason: RejectReason::ClapOutsideCommit
        }]
    );
    assert!(ben_inbox.drain().is_empty());
    assert_eq!(hub.head(), 1);
}

#[test]
fn presence_is_relayed_to_the_others() {
    let mut hub = hub();
    let (ann, ann_inbox, _) = join(&mut hub, 1);
    let (_ben, ben_inbox, _) = join(&mut hub, 2);
    ann_inbox.drain();
    ben_inbox.drain();

    let presence = Presence {
        editing: Some(EntityRef::Track(TrackId(EntityId(1)))),
        playhead: None,
    };
    hub.handle(ann, ClientMsg::Presence(presence));
    assert!(ann_inbox.drain().is_empty());
    assert_eq!(ben_inbox.drain(), [ServerMsg::Presence { actor: ann, presence }]);
}

#[test]
fn a_resuming_client_gets_what_it_missed() {
    let mut hub = hub();
    let (ann, _, ann_welcome) = join(&mut hub, 1);
    let (ben, _, _) = join(&mut hub, 2);
    hub.submit(ann, rename(1));
    // The connection breaks. Two more of Ann's edits still arrive, one of them refused; she
    // sees neither outcome.
    hub.submit(ann, rename(2));
    hub.submit(ann, clap(3));
    hub.leave(ann);
    hub.submit(ben, rename(1));

    let resume = Resume {
        prev_actor: ann,
        session_secret: ann_welcome.session_secret,
        last_seq: 1,
    };
    let (ann2, _, welcome) = rejoin(&mut hub, 1, Some(resume));
    assert_ne!(ann2, ann);
    assert_eq!(welcome.user, ann_welcome.user);
    assert_eq!(welcome.head_seq, 3);
    let SyncData::Events(events) = welcome.sync else {
        panic!("expected events")
    };
    assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [2, 3]);
    assert_eq!(
        welcome.acked,
        Some(3),
        "everything up to the refused edit has been dealt with"
    );
}

#[test]
fn a_resume_replaces_the_connection_it_names() {
    let mut hub = hub();
    let (ann, _, ann_welcome) = join(&mut hub, 1);
    hub.submit(ann, rename(1));

    // The server has not noticed the old connection is dead when the new one arrives.
    let resume = Resume {
        prev_actor: ann,
        session_secret: ann_welcome.session_secret,
        last_seq: 0,
    };
    let (ann2, _, welcome) = rejoin(&mut hub, 1, Some(resume));
    assert_eq!(welcome.acked, Some(1));
    assert_eq!(hub.peers().iter().map(|p| p.actor).collect::<Vec<_>>(), [ann2]);

    // Stragglers from the old connection are dropped; the client sends them again.
    hub.submit(ann, rename(2));
    assert_eq!(hub.head(), 1);
}

#[test]
fn a_resume_that_does_not_check_out_is_a_fresh_join() {
    let mut hub = hub();
    let (ann, _, ann_welcome) = join(&mut hub, 1);
    hub.submit(ann, rename(1));

    for resume in [
        Resume {
            prev_actor: ann,
            session_secret: ann_welcome.session_secret ^ 1,
            last_seq: 1,
        },
        Resume {
            prev_actor: ActorId(ann.0 + 100),
            session_secret: ann_welcome.session_secret,
            last_seq: 1,
        },
        // More than this server ever sent: its data has been replaced.
        Resume {
            prev_actor: ann,
            session_secret: ann_welcome.session_secret,
            last_seq: 9,
        },
    ] {
        let (_, _, welcome) = rejoin(&mut hub, 1, Some(resume));
        assert_eq!(welcome.sync, SyncData::Snapshot(hub.doc().clone()));
        assert_eq!(welcome.acked, None);
    }
    assert!(
        hub.peers().iter().any(|p| p.actor == ann),
        "the forged resume did not kick anyone"
    );
}

#[test]
fn a_client_far_behind_gets_the_document_instead_of_the_log() {
    let mut hub = hub();
    let (ann, _, ann_welcome) = join(&mut hub, 1);
    let (ben, _, _) = join(&mut hub, 2);
    hub.leave(ann);
    for n in 1..=5001 {
        hub.submit(ben, rename(n));
    }
    let resume = |last_seq| Resume {
        prev_actor: ann,
        session_secret: ann_welcome.session_secret,
        last_seq,
    };
    let (ann2, _, welcome) = rejoin(&mut hub, 1, Some(resume(0)));
    assert_eq!(welcome.sync, SyncData::Snapshot(hub.doc().clone()));

    // Further back than memory reaches, but within the limit: read from the store.
    let ann2_secret = welcome.session_secret;
    hub.leave(ann2);
    let resume = Resume {
        prev_actor: ann2,
        session_secret: ann2_secret,
        last_seq: 500,
    };
    let (_, _, welcome) = rejoin(&mut hub, 1, Some(resume));
    let SyncData::Events(events) = welcome.sync else {
        panic!("expected events")
    };
    assert_eq!(events.len(), 4501);
    assert_eq!((events[0].seq, events[4500].seq), (501, 5001));
}

#[test]
fn a_client_that_cannot_keep_up_is_dropped() {
    let mut hub = hub();
    let (ann, ann_inbox, _) = join(&mut hub, 1);

    let room = Arc::new(Mutex::new(2));
    let left = room.clone();
    let slow: Sink = Box::new(move |_| {
        let mut left = left.lock().unwrap();
        *left -= 1;
        *left >= 0
    });
    // The welcome and the peer list fit.
    let ben = hub.join(hello(&hub, 2, None), slow).unwrap();
    assert_eq!(hub.peers().len(), 2);
    ann_inbox.drain();

    hub.submit(ann, rename(1));
    assert_eq!(hub.peers().iter().map(|p| p.actor).collect::<Vec<_>>(), [ann]);
    assert!(
        matches!(&ann_inbox.drain()[..], [ServerMsg::Event(_), ServerMsg::Peers(peers)] if peers.len() == 1)
    );
    // It may come back and resume.
    hub.submit(ben, rename(1));
    assert_eq!(hub.head(), 1);
}

#[test]
fn the_project_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("project.db");
    let mut hub = Hub::open(Store::open(&path).unwrap()).unwrap();
    let (ann, _, ann_welcome) = join(&mut hub, 1);
    for n in 1..=1005 {
        hub.submit(ann, rename(n));
    }
    hub.submit(ann, clap(1006));
    let (doc, token) = (hub.doc().clone(), hub.token().to_string());
    drop(hub);

    let mut hub = Hub::open(Store::open(&path).unwrap()).unwrap();
    assert_eq!(hub.head(), 1005);
    assert_eq!(hub.doc(), &doc);
    assert_eq!(hub.token(), token);

    // Sessions outlive the restart too. What the server refused before it has forgotten.
    let resume = Resume {
        prev_actor: ann,
        session_secret: ann_welcome.session_secret,
        last_seq: 1000,
    };
    let (ann2, _, welcome) = rejoin(&mut hub, 1, Some(resume));
    assert!(ann2.0 > ann.0, "actor IDs are never handed out twice");
    assert_eq!(welcome.user, ann_welcome.user);
    assert_eq!(welcome.acked, Some(1005));
    let SyncData::Events(events) = welcome.sync else {
        panic!("expected events")
    };
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [1001, 1002, 1003, 1004, 1005]
    );
}

#[test]
fn a_store_in_another_format_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("project.db");
    let store = Store::open(&path).unwrap();
    store.set_meta("format", b"0/0").unwrap();
    drop(store);
    let error = Store::open(&path).err().unwrap();
    assert!(
        format!("{error:#}").contains("move the data directory away"),
        "{error:#}"
    );
}
