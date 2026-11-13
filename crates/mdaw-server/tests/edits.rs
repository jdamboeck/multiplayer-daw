//! Blobs and the edits that name them, at the hub, without a network.

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
        id: TxnId::random(),
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

fn plugin(id: u64, state: Option<BlobHash>) -> Device {
    Device::new(
        DeviceId(EntityId(1 << 32 | id)),
        DeviceKind::Plugin {
            format: PluginFormat::Clap,
            id: "org.example.synth".into(),
            name: "Synth".into(),
            state,
        },
    )
}

fn state_of(hub: &Hub, device: DeviceId) -> Option<BlobHash> {
    match &hub.doc().track(TRACK)?.content.device(device)?.kind {
        DeviceKind::Plugin { state, .. } => *state,
        DeviceKind::Builtin { .. } => None,
    }
}

/// A plugin goes onto a track, and gets its settings, by plain transactions: nothing is locked,
/// but every state blob an edit names must be on the server first.
#[test]
fn a_plugin_and_its_state_arrive_by_ordinary_edits_once_the_state_is_uploaded() {
    let (mut hub, ann, ben) = setup(Store::in_memory().unwrap());
    let device = plugin(50, None);
    let id = device.id;
    let add = Txn {
        id: TxnId::from(2),
        base_seq: 1,
        ops: vec![Op::DeviceAdd {
            track: TRACK,
            device,
            after: None,
        }],
    };
    hub.submit(ann.actor, add);
    assert_eq!(ben.inbox.events().len(), 1, "everyone gets the new device");
    assert_eq!(ann.inbox.events().len(), 1);

    // A state the server does not have is turned down, for the sender alone.
    let bytes = b"the plugin's own settings".to_vec();
    let blob = blob_hash(&bytes);
    let set = |n: u128| Txn {
        id: TxnId::from(n),
        base_seq: 2,
        ops: vec![Op::DeviceSetState {
            track: TRACK,
            id,
            state: Some(blob),
        }],
    };
    hub.submit(ann.actor, set(3));
    assert_eq!(
        ann.inbox.drain(),
        [ServerMsg::Rejected {
            txn: TxnId::from(3),
            reason: RejectReason::MissingBlob(blob)
        }]
    );
    assert!(ben.inbox.drain().is_empty());
    assert_eq!(state_of(&hub, id), None);
    // So is a plugin added with such a state already.
    let with_state = Txn {
        id: TxnId::from(4),
        base_seq: 2,
        ops: vec![Op::DeviceAdd {
            track: TRACK,
            device: plugin(51, Some(blob)),
            after: None,
        }],
    };
    hub.submit(ben.actor, with_state.clone());
    assert_eq!(
        ben.inbox.drain(),
        [ServerMsg::Rejected {
            txn: TxnId::from(4),
            reason: RejectReason::MissingBlob(blob)
        }]
    );

    // Uploaded, the same edits go through, from either client.
    assert!(hub.blobs().put(&blob, &bytes).unwrap());
    hub.submit(ann.actor, set(5));
    let [event] = &ben.inbox.events()[..] else {
        panic!("Ben hears of Ann's state")
    };
    assert_eq!(event.user, ann.user);
    assert_eq!(state_of(&hub, id), Some(blob));
    ann.inbox.drain();
    hub.submit(
        ben.actor,
        Txn {
            id: TxnId::from(5),
            base_seq: 3,
            ..with_state
        },
    );
    assert_eq!(ann.inbox.events().len(), 1);
    assert_eq!(hub.doc().track(TRACK).unwrap().content.devices.len(), 2);

    // And the track with plugins takes ordinary edits from anyone.
    let notes = Txn {
        id: TxnId::from(6),
        base_seq: 4,
        ops: vec![note(1)],
    };
    hub.submit(ben.actor, notes);
    assert_eq!(ann.inbox.events().len(), 1);
    ben.inbox.drain();
    let bypass = Txn {
        id: TxnId::from(7),
        base_seq: 5,
        ops: vec![Op::DeviceSetBypass {
            track: TRACK,
            id,
            bypass: true,
        }],
    };
    hub.submit(ann.actor, bypass);
    assert_eq!(ben.inbox.events().len(), 1);
    assert!(matches!(
        hub.doc().track(TRACK).unwrap().content.device(id),
        Some(Device { bypass: true, .. })
    ));
}

/// Edits that displaced someone else's are what the history shows; they survive a restart and
/// come with the document to whoever joins next.
#[test]
fn displaced_edits_survive_a_restart_and_new_clients_see_them_in_the_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("project.db");
    let (mut hub, ann, ben) = setup(Store::open(&path).unwrap());
    let rename = |n: u128, name: &str| Txn {
        id: TxnId::from(n),
        base_seq: 1,
        ops: vec![Op::ClipSet {
            track: TRACK,
            id: CLIP,
            prop: ClipProp::Name(name.into()),
        }],
    };
    hub.submit(ann.actor, rename(2, "ann's"));
    // Ben had not seen Ann's name when he gave his.
    hub.submit(ben.actor, rename(1, "ben's"));
    hub.submit(
        ann.actor,
        Txn {
            id: TxnId::from(3),
            base_seq: 3,
            ops: vec![note(1)],
        },
    );
    let doc = hub.doc().clone();
    drop(hub);

    let mut hub = Hub::open(Store::open(&path).unwrap(), Blobs::in_memory()).unwrap();
    assert_eq!(hub.doc(), &doc);
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
    let [entry] = &welcome.history[..] else {
        panic!("one entry; plain edits are not news: {:?}", welcome.history)
    };
    assert_eq!((entry.seq, entry.user), (3, ben.user));
    assert_eq!(entry.superseded[0].lost_user, ann.user);
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
fn uploads_of_one_blob_at_once_do_not_trip_over_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blobs");
    std::fs::create_dir_all(&path).unwrap();
    // Left behind by a crash.
    std::fs::write(path.join("x.part"), b"half").unwrap();
    let blobs = Blobs::open(&path).unwrap();
    assert!(!path.join("x.part").exists());

    let bytes: Arc<[u8]> = vec![3u8; 4 << 20].into();
    let hash = blob_hash(&bytes);
    let uploads: Vec<_> = (0..8)
        .map(|_| {
            let (blobs, bytes) = (blobs.clone(), bytes.clone());
            std::thread::spawn(move || blobs.put(&hash, &bytes).unwrap())
        })
        .collect();
    for upload in uploads {
        assert!(upload.join().unwrap());
    }
    assert_eq!(blobs.get(&hash).unwrap().as_deref(), Some(&bytes[..]));
    let files: Vec<_> = std::fs::read_dir(&path).unwrap().collect();
    assert_eq!(files.len(), 1, "no halves left");
}

#[test]
fn a_clip_does_not_grow_past_its_limit_one_note_at_a_time() {
    let (mut hub, ann, _ben) = setup(Store::in_memory().unwrap());
    let add = |n: u64| Op::NoteAdd {
        track: TRACK,
        clip: CLIP,
        id: NoteId(EntityId(1 << 32 | (1000 + n))),
        note: Note {
            start: Tick(n as i64 % 3840),
            dur: Tick(10),
            pitch: 60,
            vel: 100,
        },
    };
    let txn = |id: u128, ops: Vec<Op>| Txn {
        id: TxnId::from(id),
        base_seq: 1,
        ops,
    };
    let notes = |hub: &Hub| hub.doc().track(TRACK).unwrap().content.clips[&CLIP].notes.len();

    // Over the limit in one transaction: none of it lands.
    let t = txn(2, (0..50_001).map(add).collect());
    hub.submit(ann.actor, t);
    assert_eq!(notes(&hub), 0);
    // Up to it, then one more.
    let t = txn(3, (0..50_000).map(add).collect());
    hub.submit(ann.actor, t);
    assert_eq!(notes(&hub), 50_000);
    ann.inbox.drain();
    let t = txn(4, vec![add(60_000)]);
    hub.submit(ann.actor, t);
    assert_eq!(notes(&hub), 50_000);
    let refused = ann.inbox.drain().into_iter().any(|m| {
        matches!(
            m,
            ServerMsg::Rejected {
                txn,
                reason: RejectReason::Invalid(_)
            } if txn == TxnId::from(4)
        )
    });
    assert!(refused);
    // Changing a note that is there is still fine.
    let t = txn(5, vec![add(5)]);
    hub.submit(ann.actor, t);
    assert!(ann.inbox.drain().iter().any(|m| matches!(m, ServerMsg::Event(_))));
}
