//! Replicas against the hub, with messages delayed, transactions sent twice, connections cut
//! (acknowledgements lost with them) and resumed by catch-up or by snapshot, and the server
//! restarted, all at random. Whatever happens on the way, once everything is delivered every
//! replica holds the server's document, the log holds every transaction of a user at most once,
//! and every transaction a client made is in the log exactly once or was refused.

use mdaw_model::*;
use mdaw_proto::*;
use mdaw_server::{Blobs, Hub, Sink, Store};
use mdaw_sync::Replica;
use proptest::prelude::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A client this far behind gets a snapshot rather than the events; small, so schedules reach
/// that path often.
const CATCH_UP_LIMIT: u64 = 6;

type Queue = Arc<Mutex<VecDeque<ServerMsg>>>;

/// One connection. Messages keep their order within it, as on a stream; how the two directions
/// and the connections of different clients interleave is up to the test.
struct Conn {
    actor: ActorId,
    down: Queue,
    up: VecDeque<ClientMsg>,
    /// The client has processed the server's welcome and may send.
    welcomed: bool,
}

struct Client {
    key: u8,
    user: UserId,
    replica: Replica,
    secret: u64,
    conn: Option<Conn>,
    /// Every transaction this client made, in order.
    created: Vec<Txn>,
    /// Those the server refused.
    rejected: HashSet<TxnId>,
}

struct World {
    hub: Hub,
    /// Where the project lives, so the server can be restarted on it.
    _dir: tempfile::TempDir,
    path: PathBuf,
    blobs: Blobs,
    clients: Vec<Client>,
    /// The server's document after each event, by sequence number.
    history: Vec<Project>,
}

fn sink(queue: &Queue) -> Sink {
    let queue = queue.clone();
    Box::new(move |msg| {
        queue.lock().unwrap().push_back(msg);
        true
    })
}

fn open(path: &std::path::Path, blobs: &Blobs) -> Hub {
    Hub::open(Store::open(path).unwrap(), blobs.clone())
        .unwrap()
        .with_catch_up_limit(CATCH_UP_LIMIT)
}

impl World {
    fn new(clients: u8) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("project.db");
        let blobs = Blobs::in_memory();
        let mut hub = open(&path, &blobs);
        // The one plugin state every client has uploaded.
        assert!(hub.blobs().put(&stored(), STORED).unwrap());
        let history = vec![hub.doc().clone()];
        let clients = (0..clients)
            .map(|key| {
                let down = Queue::default();
                let hello = Hello {
                    proto_version: PROTO_VERSION,
                    token: hub.token().to_string(),
                    user_key: [key; 16],
                    name: format!("client {key}"),
                    resume: None,
                };
                let actor = hub.join(hello, sink(&down)).unwrap();
                let Some(ServerMsg::Welcome(welcome)) = down.lock().unwrap().pop_front() else {
                    panic!("no welcome")
                };
                let SyncData::Snapshot(project) = welcome.sync else {
                    panic!("no snapshot")
                };
                Client {
                    key,
                    user: welcome.user,
                    replica: Replica::new(actor, project, welcome.head_seq),
                    secret: welcome.session_secret,
                    conn: Some(Conn {
                        actor,
                        down,
                        up: VecDeque::new(),
                        welcomed: true,
                    }),
                    created: Vec::new(),
                    rejected: HashSet::new(),
                }
            })
            .collect();
        Self {
            hub,
            _dir: dir,
            path,
            blobs,
            clients,
            history,
        }
    }

    fn send(&mut self, client: usize, txn: Option<Txn>) {
        let Some(txn) = txn else { return };
        let c = &mut self.clients[client];
        c.created.push(txn.clone());
        // Offline edits wait in the replica's pending queue.
        if let Some(conn) = &mut c.conn
            && conn.welcomed
        {
            conn.up.push_back(ClientMsg::Submit(txn));
        }
    }

    /// The client sends one of its transactions once more, as it is, whatever became of it.
    fn duplicate(&mut self, client: usize, which: usize) {
        let c = &mut self.clients[client];
        if let Some(conn) = &mut c.conn
            && conn.welcomed
            && !c.created.is_empty()
        {
            let txn = c.created[which % c.created.len()].clone();
            conn.up.push_back(ClientMsg::Submit(txn));
        }
    }

    /// The server goes down and comes back on the same data. Every connection is lost, with
    /// whatever was under way on it.
    fn restart(&mut self) {
        let doc = self.hub.doc().clone();
        let head = self.hub.head();
        self.hub = open(&self.path, &self.blobs);
        assert_eq!((self.hub.head(), self.hub.doc()), (head, &doc), "nothing lost");
        for c in &mut self.clients {
            c.conn = None;
        }
    }

    /// The server receives the client's next message.
    fn up(&mut self, client: usize) -> bool {
        let Some(conn) = &mut self.clients[client].conn else {
            return false;
        };
        let Some(msg) = conn.up.pop_front() else {
            return false;
        };
        self.hub.handle(conn.actor, msg);
        if self.hub.head() as usize == self.history.len() {
            self.history.push(self.hub.doc().clone());
        }
        true
    }

    /// The client receives the server's next message.
    fn down(&mut self, client: usize) -> bool {
        let c = &mut self.clients[client];
        let Some(conn) = &mut c.conn else { return false };
        let Some(msg) = conn.down.lock().unwrap().pop_front() else {
            return false;
        };
        match msg {
            ServerMsg::Welcome(welcome) => {
                assert!(!conn.welcomed);
                assert_eq!(welcome.actor, conn.actor);
                assert_eq!(welcome.user, c.user);
                let resend = c
                    .replica
                    .resync(welcome.actor, welcome.head_seq, welcome.sync)
                    .expect("the welcome continues where the replica is");
                conn.up.extend(resend.into_iter().map(ClientMsg::Submit));
                conn.welcomed = true;
                c.secret = welcome.session_secret;
            }
            ServerMsg::Event(event) => c.replica.on_event(&event).expect("events arrive in order"),
            ServerMsg::Rejected { txn, .. } => {
                c.replica.on_rejected(txn);
                c.rejected.insert(txn);
            }
            ServerMsg::Accepted { txn, seq } => {
                assert!(seq <= self.hub.head());
                c.replica.on_accepted(txn, seq);
            }
            ServerMsg::Peers(_) | ServerMsg::Presence { .. } => {}
            ServerMsg::Refused(reason) => panic!("refused: {reason}"),
        }
        assert_eq!(
            c.replica.confirmed(),
            &self.history[c.replica.seq() as usize],
            "client {client} at event {} is not where the server was",
            c.replica.seq()
        );
        true
    }

    /// The connection breaks. The first `flush` messages under way to the server still arrive;
    /// everything else in flight is lost. `noticed` is whether the server sees the break before
    /// the client is back.
    fn cut(&mut self, client: usize, flush: usize, noticed: bool) {
        for _ in 0..flush {
            self.up(client);
        }
        if let Some(conn) = self.clients[client].conn.take()
            && noticed
        {
            self.hub.leave(conn.actor);
        }
    }

    /// `forget` stands for a client whose session the server no longer accepts: it gets the
    /// whole document and sends everything pending again.
    fn connect(&mut self, client: usize, forget: bool) {
        let c = &mut self.clients[client];
        if c.conn.is_some() {
            return;
        }
        let resume = Resume {
            prev_actor: c.replica.actor(),
            session_secret: c.secret ^ u64::from(forget),
            last_seq: c.replica.seq(),
        };
        let hello = Hello {
            proto_version: PROTO_VERSION,
            token: self.hub.token().to_string(),
            user_key: [c.key; 16],
            name: format!("client {}", c.key),
            resume: Some(resume),
        };
        let down = Queue::default();
        let actor = self.hub.join(hello, sink(&down)).unwrap();
        c.conn = Some(Conn {
            actor,
            down,
            up: VecDeque::new(),
            welcomed: false,
        });
    }

    fn edit(&mut self, client: usize, kind: u8, a: usize, b: u8) {
        let replica = &mut self.clients[client].replica;
        let view = replica.view().clone();
        let ops = edit(&view, replica.ids(), kind, a, b);
        let txn = replica.submit(ops);
        self.send(client, txn);
    }

    fn run(&mut self, action: &Action) {
        match *action {
            Action::Edit { client, kind, a, b } => self.edit(client, kind, a, b),
            Action::Undo { client } => {
                let txn = self.clients[client].replica.undo();
                self.send(client, txn);
            }
            Action::Redo { client } => {
                let txn = self.clients[client].replica.redo();
                self.send(client, txn);
            }
            Action::Up { client } => {
                self.up(client);
            }
            Action::Down { client } => {
                self.down(client);
            }
            Action::Cut {
                client,
                flush,
                noticed,
            } => self.cut(client, flush, noticed),
            Action::Connect { client, forget } => self.connect(client, forget),
            Action::Duplicate { client, which } => self.duplicate(client, which),
            Action::Restart => self.restart(),
        }
    }

    /// Reconnects everyone and delivers everything.
    fn settle(&mut self) {
        for client in 0..self.clients.len() {
            self.connect(client, false);
        }
        loop {
            let mut moved = false;
            for client in 0..self.clients.len() {
                moved |= self.up(client) | self.down(client);
            }
            if !moved {
                break;
            }
        }
    }

    /// Delivers everything between the server and one client.
    fn settle_client(&mut self, client: usize) {
        while self.up(client) | self.down(client) {}
    }

    fn assert_converged(&self) {
        for (n, c) in self.clients.iter().enumerate() {
            assert_eq!(
                c.replica.pending().len(),
                0,
                "client {n} still has unconfirmed edits"
            );
            assert_eq!(c.replica.seq(), self.hub.head(), "client {n}");
            assert_eq!(c.replica.view(), self.hub.doc(), "client {n}");
            assert_eq!(c.replica.confirmed(), self.hub.doc(), "client {n}");
        }
        assert_eq!(
            self.hub.peers().iter().filter(|p| self.is_live(p.actor)).count(),
            self.clients.len()
        );

        // The log on disk folds into the document, and no transaction is in it twice.
        let log = Store::open(&self.path)
            .unwrap()
            .events_after(0, u64::MAX >> 1)
            .unwrap();
        let mut doc = self.history[0].clone();
        let mut times: HashMap<(UserId, TxnId), usize> = HashMap::new();
        for (n, event) in log.iter().enumerate() {
            assert_eq!(event.seq, n as u64 + 1);
            apply_event(&mut doc, event);
            *times.entry((event.user, event.txn)).or_default() += 1;
        }
        assert_eq!(log.len() as u64, self.hub.head());
        assert_eq!(&doc, self.hub.doc());
        for (&(user, txn), &n) in &times {
            assert_eq!(n, 1, "transaction {txn} of {user:?} is in the log {n} times");
        }

        // Every transaction a client made either landed once or was refused, not both.
        for (n, c) in self.clients.iter().enumerate() {
            for txn in &c.created {
                let landed = times.contains_key(&(c.user, txn.id));
                let refused = c.rejected.contains(&txn.id);
                assert!(
                    landed != refused,
                    "client {n}'s transaction {}: in the log {landed}, refused {refused}",
                    txn.id
                );
            }
        }
    }

    fn is_live(&self, actor: ActorId) -> bool {
        self.clients
            .iter()
            .any(|c| c.conn.as_ref().is_some_and(|conn| conn.actor == actor))
    }
}

fn tracks(project: &Project) -> Vec<TrackId> {
    project.track_order.iter().copied().collect()
}

fn clips(project: &Project) -> Vec<(TrackId, ClipId)> {
    let mut found = Vec::new();
    for (track, t) in project.ordered_tracks() {
        found.extend(t.content.clips.keys().map(|&clip| (track, clip)));
    }
    found
}

fn notes(project: &Project) -> Vec<(TrackId, ClipId, NoteId)> {
    let mut found = Vec::new();
    for (track, t) in project.ordered_tracks() {
        for (&clip, c) in &t.content.clips {
            found.extend(c.notes.keys().map(|&note| (track, clip, note)));
        }
    }
    found
}

fn plugins(project: &Project) -> Vec<(TrackId, DeviceId)> {
    let mut found = Vec::new();
    for (track, t) in project.ordered_tracks() {
        let plugins = t.content.devices.iter();
        let plugins = plugins.filter(|d| matches!(d.kind, DeviceKind::Plugin { .. }));
        found.extend(plugins.map(|d| (track, d.id)));
    }
    found
}

/// Bytes of a plugin state that is on the server, and the hash of one that is not.
const STORED: &[u8] = b"plugin state";
fn stored() -> BlobHash {
    blob_hash(STORED)
}
fn missing() -> BlobHash {
    blob_hash(b"never uploaded")
}

fn plugin(ids: &mut IdGen, state: Option<BlobHash>) -> Device {
    Device::new(
        ids.device(),
        DeviceKind::Plugin {
            format: PluginFormat::Clap,
            id: "com.example".into(),
            name: "Synth".into(),
            state,
        },
    )
}

fn pick<T: Copy>(from: &[T], a: usize) -> Option<T> {
    (!from.is_empty()).then(|| from[a % from.len()])
}

/// An edit a user could make to `view`. Empty when the document has nothing to make it on.
fn edit(view: &Project, ids: &mut IdGen, kind: u8, a: usize, b: u8) -> Vec<Op> {
    let track = pick(&tracks(view), a);
    let clip = pick(&clips(view), a);
    let note = pick(&notes(view), a);
    let device = pick(&plugins(view), a);
    let op = match kind % 17 {
        0 => Some(Op::TrackAdd {
            id: ids.track(),
            shell: TrackShell {
                name: format!("track {b}"),
                color: u32::from(b),
            },
            content: TrackContent::default(),
            after: track,
        }),
        1 => track.map(|id| Op::TrackRemove { id }),
        2 => track.map(|track| Op::ClipAdd {
            track,
            id: ids.clip(),
            clip: Clip {
                start: Tick::bars(i64::from(b % 8)),
                length: Tick::bars(1 + i64::from(b % 4)),
                name: String::new(),
                notes: Default::default(),
            },
        }),
        3 => clip.map(|(track, id)| Op::ClipRemove { track, id }),
        4 | 5 => clip.map(|(track, clip)| Op::NoteAdd {
            track,
            clip,
            id: ids.note(),
            note: Note {
                start: Tick::beats(i64::from(b % 16)),
                dur: Tick::beats(1),
                pitch: 36 + b % 60,
                vel: 1 + b % 127,
            },
        }),
        6 => note.map(|(track, clip, id)| Op::NoteSet {
            track,
            clip,
            id,
            prop: NoteProp::Pitch(36 + b % 60),
        }),
        7 => note.map(|(track, clip, id)| Op::NoteRemove { track, clip, id }),
        8 => clip.map(|(track, id)| Op::ClipSet {
            track,
            id,
            prop: ClipProp::Start(Tick::bars(i64::from(b % 8))),
        }),
        9 => track.map(|track| Op::TrackSetMix {
            track,
            prop: MixProp::Gain(f32::from(b % 8) / 8.0),
        }),
        10 => view.tempo.keys().next().map(|&id| Op::TempoPut {
            id,
            point: TempoPoint {
                tick: Tick::ZERO,
                // 60..=300 bpm: the model's (and the audio engine's) range.
                milli_bpm: 60_000 + u32::from(b % 241) * 1000,
            },
        }),
        11 => track.map(|id| Op::TrackMove {
            id,
            after: pick(&tracks(view), usize::from(b)).filter(|&t| t != id),
        }),
        12 => Some(Op::ProjectSetName {
            name: format!("song {b}"),
        }),
        // A plugin, with or without settings, is an ordinary edit.
        14 => track.map(|track| Op::DeviceAdd {
            track,
            device: plugin(ids, b.is_multiple_of(2).then(stored)),
            after: None,
        }),
        15 => device.map(|(track, id)| Op::DeviceSetState {
            track,
            id,
            state: (!b.is_multiple_of(3)).then(stored),
        }),
        16 => device.map(|(track, id)| Op::DeviceSetParam {
            track,
            id,
            param: u32::from(b % 4),
            value: Some(f64::from(b)),
        }),
        // A plugin whose settings never reached the server is refused, and with it the whole
        // edit.
        _ => {
            let device = plugin(ids, Some(missing()));
            let add = track.map(|track| Op::DeviceAdd {
                track,
                device,
                after: None,
            });
            return add
                .into_iter()
                .chain([Op::MasterSetGain {
                    gain: f32::from(b % 8) / 8.0,
                }])
                .collect();
        }
    };
    op.into_iter().collect()
}

#[derive(Clone, Debug)]
enum Action {
    Edit {
        client: usize,
        kind: u8,
        a: usize,
        b: u8,
    },
    Undo {
        client: usize,
    },
    Redo {
        client: usize,
    },
    Up {
        client: usize,
    },
    Down {
        client: usize,
    },
    Cut {
        client: usize,
        flush: usize,
        noticed: bool,
    },
    Connect {
        client: usize,
        forget: bool,
    },
    /// The client sends one of the transactions it made again, unchanged.
    Duplicate {
        client: usize,
        which: usize,
    },
    Restart,
}

const CLIENTS: usize = 3;

fn action() -> impl Strategy<Value = Action> {
    let client = 0..CLIENTS;
    prop_oneof![
        6 => (client.clone(), any::<u8>(), any::<usize>(), any::<u8>())
            .prop_map(|(client, kind, a, b)| Action::Edit { client, kind, a, b }),
        1 => client.clone().prop_map(|client| Action::Undo { client }),
        1 => client.clone().prop_map(|client| Action::Redo { client }),
        5 => client.clone().prop_map(|client| Action::Up { client }),
        5 => client.clone().prop_map(|client| Action::Down { client }),
        1 => (client.clone(), 0..4usize, any::<bool>())
            .prop_map(|(client, flush, noticed)| Action::Cut { client, flush, noticed }),
        2 => (client.clone(), prop::bool::weighted(0.15)).prop_map(|(client, forget)| Action::Connect { client, forget }),
        1 => (client, any::<usize>()).prop_map(|(client, which)| Action::Duplicate { client, which }),
        1 => Just(Action::Restart),
    ]
}

/// Something to edit from the start: a track with a clip and a few notes, known to everyone.
fn seeded() -> World {
    let mut world = World::new(CLIENTS as u8);
    for (kind, b) in [(0, 0), (2, 0), (4, 0), (4, 5), (4, 9), (0, 1), (2, 2)] {
        world.edit(0, kind, 0, b);
    }
    world.settle();
    world.assert_converged();
    assert_eq!(world.hub.head(), 7);
    world
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

    #[test]
    fn replicas_converge(actions in prop::collection::vec(action(), 0..160)) {
        let mut world = seeded();
        for action in &actions {
            world.run(action);
        }
        world.settle();
        world.assert_converged();
    }
}

#[test]
fn offline_edits_arrive_after_the_reconnect() {
    let mut world = seeded();
    world.cut(1, 0, true);
    world.edit(1, 12, 0, 7);
    world.edit(0, 9, 0, 4);
    world.settle();
    world.assert_converged();
    assert_eq!(world.hub.doc().name, "song 7");
}

#[test]
fn an_edit_confirmed_while_the_client_was_away_is_not_made_twice() {
    let mut world = seeded();
    let before = world.hub.head();
    world.edit(1, 12, 0, 7);
    // It reaches the server; the confirmation does not reach the client.
    world.cut(1, 1, false);
    assert_eq!(world.hub.head(), before + 1);
    assert_eq!(world.clients[1].replica.pending().len(), 1);
    world.settle();
    world.assert_converged();
    assert_eq!(world.hub.head(), before + 1);
}

#[test]
fn an_edit_confirmed_while_the_client_was_away_is_not_made_twice_after_a_snapshot() {
    let mut world = seeded();
    let before = world.hub.head();
    world.edit(1, 12, 0, 7);
    world.cut(1, 1, false);
    // Others edit meanwhile, past what a catch-up covers.
    for b in 0..CATCH_UP_LIMIT as u8 + 1 {
        world.edit(0, 9, 0, b);
    }
    world.settle_client(0);
    world.connect(1, false);
    let Some(ServerMsg::Welcome(welcome)) = world.clients[1]
        .conn
        .as_ref()
        .unwrap()
        .down
        .lock()
        .unwrap()
        .front()
        .cloned()
    else {
        panic!("no welcome")
    };
    assert!(matches!(welcome.sync, SyncData::Snapshot(_)), "the snapshot path");
    world.settle();
    world.assert_converged();
    assert_eq!(world.hub.head(), before + 1 + CATCH_UP_LIMIT + 1);
    assert_eq!(world.hub.doc().name, "song 7");
}

#[test]
fn an_edit_taken_just_before_the_server_restarts_is_not_made_twice() {
    let mut world = seeded();
    let before = world.hub.head();
    world.edit(1, 12, 0, 7);
    world.up(1);
    world.edit(1, 12, 0, 8);
    // The server takes the first, and goes down before anyone hears of it.
    world.restart();
    world.settle();
    world.assert_converged();
    assert_eq!(world.hub.head(), before + 2);
    assert_eq!(world.hub.doc().name, "song 8");
}

/// Two clients change the same track, which has a plugin, at the same time: its notes, its
/// mix and the plugin's settings. Nobody holds a lock; everyone ends with the server's document.
#[test]
fn concurrent_edits_of_a_track_with_a_plugin_converge() {
    let mut world = seeded();
    // With its settings.
    world.edit(0, 14, 0, 0);
    world.settle();
    world.assert_converged();
    let (track, device) = plugins(world.hub.doc())[0];
    assert_eq!(Some(track), world.hub.doc().track_order.front().copied());
    // Clients 1 and 2 edit without seeing each other's edits.
    for (client, b) in [(1, 5), (2, 9)] {
        world.edit(client, 16, 0, b);
        world.edit(client, 6, 0, b);
        world.edit(client, 9, 0, b);
        // Client 1 keeps the settings as they are, which sends nothing; client 2 clears them.
        world.edit(client, 15, 0, b);
    }
    world.settle();
    world.assert_converged();
    let doc = world.hub.doc();
    let content = &doc.track(track).unwrap().content;
    let plugin = content.device(device).unwrap();
    // Each pair of edits writes the same property; the later one (client 2's) wins each.
    assert!(matches!(&plugin.kind, DeviceKind::Plugin { .. }));
    assert_eq!(plugin.params.get(&1), Some(&9.0));
    assert!(matches!(plugin.kind, DeviceKind::Plugin { state: None, .. }));
    assert_eq!(content.gain, 1.0 / 8.0);
    let (_, clip, note) = notes(doc)[0];
    assert_eq!(content.clips[&clip].notes[&note].pitch, 36 + 9);
}

#[test]
fn a_refused_edit_is_rolled_back_as_a_whole() {
    let mut world = seeded();
    let gain = world.hub.doc().master_gain;
    world.edit(2, 13, 0, 3);
    assert_ne!(
        world.clients[2].replica.view().master_gain,
        gain,
        "shown optimistically"
    );
    world.settle();
    world.assert_converged();
    assert_eq!(world.hub.doc().master_gain, gain);
}
