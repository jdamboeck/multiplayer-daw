//! The project hub without a network: clients are queues.

use mdaw_model::*;
use mdaw_proto::*;
use mdaw_server::{Blobs, Hub, Sink, Store};
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
        id: TxnId::from(u128::from(n)),
        base_seq: 0,
        ops: vec![Op::ProjectSetName {
            name: format!("take {n}"),
        }],
    }
}

/// Adds a plugin whose state was never uploaded: the server refuses it.
fn refused(n: u64) -> Txn {
    let device = Device::new(
        DeviceId(EntityId(9)),
        DeviceKind::Plugin {
            format: PluginFormat::Clap,
            id: "x".into(),
            state: Some(missing()),
        },
    );
    Txn {
        id: TxnId::from(u128::from(n)),
        base_seq: 0,
        ops: vec![Op::DeviceAdd {
            track: TrackId(EntityId(1)),
            device,
            after: None,
        }],
    }
}

fn missing() -> BlobHash {
    blob_hash(b"never uploaded")
}

fn hub() -> Hub {
    Hub::open(Store::in_memory().unwrap(), Blobs::in_memory()).unwrap()
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
    assert_eq!((event.seq, event.actor, event.txn), (1, ann, TxnId::from(1)));
    assert_eq!(ben_inbox.drain(), [ServerMsg::Event(event.clone())]);
    assert_eq!(hub.doc().name, "take 1");

    hub.submit(ann, refused(2));
    assert_eq!(
        ann_inbox.drain(),
        [ServerMsg::Rejected {
            txn: TxnId::from(2),
            reason: RejectReason::MissingBlob(missing())
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
    hub.submit(ann, refused(3));
    hub.leave(ann);
    hub.submit(ben, rename(1));

    let resume = Resume {
        prev_actor: ann,
        session_secret: ann_welcome.session_secret,
        last_seq: 1,
    };
    let (ann2, ann2_inbox, welcome) = rejoin(&mut hub, 1, Some(resume));
    assert_ne!(ann2, ann);
    assert_eq!(welcome.user, ann_welcome.user);
    assert_eq!(welcome.head_seq, 3);
    let SyncData::Events(events) = welcome.sync else {
        panic!("expected events")
    };
    assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [2, 3]);

    // She sends both again, unchanged; each gets the answer it got the first time.
    ann2_inbox.drain();
    hub.submit(ann2, rename(2));
    hub.submit(ann2, refused(3));
    assert_eq!(
        ann2_inbox.drain(),
        [
            ServerMsg::Accepted {
                txn: TxnId::from(2),
                seq: 2
            },
            ServerMsg::Rejected {
                txn: TxnId::from(3),
                reason: RejectReason::MissingBlob(missing())
            }
        ]
    );
    assert_eq!(hub.head(), 3);
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
    let (ann2, _, _) = rejoin(&mut hub, 1, Some(resume));
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
    let mut hub = Hub::open(Store::open(&path).unwrap(), Blobs::in_memory()).unwrap();
    let (ann, _, ann_welcome) = join(&mut hub, 1);
    for n in 1..=1005 {
        hub.submit(ann, rename(n));
    }
    hub.submit(ann, refused(1006));
    let (doc, token) = (hub.doc().clone(), hub.token().to_string());
    drop(hub);

    let mut hub = Hub::open(Store::open(&path).unwrap(), Blobs::in_memory()).unwrap();
    assert_eq!(hub.head(), 1005);
    assert_eq!(hub.doc(), &doc);
    assert_eq!(hub.token(), token);

    // Sessions outlive the restart too, and so does what the server answered.
    let resume = Resume {
        prev_actor: ann,
        session_secret: ann_welcome.session_secret,
        last_seq: 1000,
    };
    let (ann2, ann2_inbox, welcome) = rejoin(&mut hub, 1, Some(resume));
    assert!(ann2.0 > ann.0, "actor IDs are never handed out twice");
    assert_eq!(welcome.user, ann_welcome.user);
    let SyncData::Events(events) = welcome.sync else {
        panic!("expected events")
    };
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [1001, 1002, 1003, 1004, 1005]
    );
    ann2_inbox.drain();
    hub.submit(ann2, rename(1005));
    hub.submit(ann2, refused(1006));
    assert!(matches!(
        &ann2_inbox.drain()[..],
        [
            ServerMsg::Accepted { seq: 1005, .. },
            ServerMsg::Rejected {
                reason: RejectReason::MissingBlob(_),
                ..
            }
        ]
    ));
    assert_eq!(hub.head(), 1005);
}

#[test]
fn events_lost_in_a_crash_are_not_taken_for_their_replacements() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("project.db");
    let mut hub = Hub::open(Store::open(&path).unwrap(), Blobs::in_memory()).unwrap();
    let (ann, _, ann_welcome) = join(&mut hub, 1);
    for n in 1..=5 {
        hub.submit(ann, rename(n));
    }
    drop(hub);
    // The machine went down before the last three events reached the disk.
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("DELETE FROM events WHERE seq > 2", []).unwrap();
    drop(db);

    // After the restart, other events take the same seqs.
    let mut hub = Hub::open(Store::open(&path).unwrap(), Blobs::in_memory()).unwrap();
    assert_eq!(hub.head(), 2);
    let (ben, _, _) = join(&mut hub, 2);
    for n in 10..=13 {
        hub.submit(ben, rename(n));
    }
    assert_eq!(hub.head(), 6);

    // Ann had 5; the 3 to 5 she has are not the server's, so she starts over.
    let resume = Resume {
        prev_actor: ann,
        session_secret: ann_welcome.session_secret,
        last_seq: 5,
    };
    let (_, _, welcome) = rejoin(&mut hub, 1, Some(resume));
    assert!(matches!(welcome.sync, SyncData::Snapshot(_)));

    // A client of the earlier run that had only what survived catches up as usual.
    let resume = Resume {
        last_seq: 2,
        ..resume
    };
    let (ann3, _, welcome) = rejoin(&mut hub, 1, Some(resume));
    let SyncData::Events(events) = welcome.sync else {
        panic!("expected events")
    };
    assert_eq!(events.len(), 4);

    // And from now on, resuming within this run is as before.
    let resume = Resume {
        prev_actor: ann3,
        session_secret: welcome.session_secret,
        last_seq: 6,
    };
    let (_, _, welcome) = rejoin(&mut hub, 1, Some(resume));
    assert!(matches!(welcome.sync, SyncData::Events(e) if e.is_empty()));
}

#[test]
fn events_lost_in_a_crash_stay_lost_through_later_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("project.db");
    let open = || Hub::open(Store::open(&path).unwrap(), Blobs::in_memory()).unwrap();
    let mut hub = open();
    let (ann, _, ann_welcome) = join(&mut hub, 1);
    for n in 1..=5 {
        hub.submit(ann, rename(n));
    }
    drop(hub);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("DELETE FROM events WHERE seq > 2", []).unwrap();
    drop(db);

    // Ann stays away while the server runs again, writes more, and is restarted cleanly.
    let mut hub = open();
    let (ben, _, ben_welcome) = join(&mut hub, 2);
    for n in 10..=20 {
        hub.submit(ben, rename(n));
    }
    drop(hub);
    let mut hub = open();
    assert_eq!(hub.head(), 13);

    // Ann's 3 to 5 are still not the server's.
    let resume = Resume {
        prev_actor: ann,
        session_secret: ann_welcome.session_secret,
        last_seq: 5,
    };
    let (_, _, welcome) = rejoin(&mut hub, 1, Some(resume));
    assert!(matches!(welcome.sync, SyncData::Snapshot(_)));

    // Ben, of the run between, had nothing that was lost, and catches up as usual.
    let resume = Resume {
        prev_actor: ben,
        session_secret: ben_welcome.session_secret,
        last_seq: 13,
    };
    let (_, _, welcome) = rejoin(&mut hub, 2, Some(resume));
    assert!(matches!(welcome.sync, SyncData::Events(e) if e.is_empty()));
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

/// Prints the tracks of a copy of a server's project. Run by hand with
/// `MDAW_PROBE_DB=<copied project.db> cargo test -p mdaw-server --test hub -- --ignored probe --nocapture`.
#[test]
#[ignore = "a tool for looking at a project"]
fn probe_a_project() {
    let Ok(path) = std::env::var("MDAW_PROBE_DB") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(
        Store::open(std::path::Path::new(&path)).unwrap(),
        Blobs::open(dir.path().to_path_buf()).unwrap(),
    )
    .unwrap();
    let store = Store::open(std::path::Path::new(&path)).unwrap();
    for event in store.events_after(0, 100_000).unwrap() {
        let ops: Vec<String> = event
            .ops
            .iter()
            .map(|op| format!("{op:?}").chars().take(90).collect())
            .collect();
        eprintln!("{} {} {:?}", event.seq, event.wall_ts / 1000 % 86400, ops);
    }
    let doc = hub.doc();
    eprintln!("master gain {}, tempo {:?}", doc.master_gain, doc.tempo);
    for (id, track) in doc.ordered_tracks() {
        let c = &track.content;
        eprintln!(
            "{:?} {:?}: gain {} pan {} mute {}",
            id, track.shell.name, c.gain, c.pan, c.mute
        );
        for d in &c.devices {
            eprintln!("    device {:?}", d.kind);
        }
        for clip in c.clips.values() {
            let notes: Vec<_> = clip
                .notes
                .values()
                .map(|n| (n.start.0, n.dur.0, n.pitch, n.vel))
                .collect();
            eprintln!("    clip at {} len {}: {:?}", clip.start.0, clip.length.0, notes);
        }
    }
}
