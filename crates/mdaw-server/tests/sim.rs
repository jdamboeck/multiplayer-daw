//! Replicas against the hub, with messages delayed and connections cut and resumed at random.
//! Whatever happens on the way, once everything is delivered every replica holds the server's
//! document.

use mdaw_model::*;
use mdaw_proto::*;
use mdaw_server::{Hub, Sink, Store};
use mdaw_sync::Replica;
use proptest::prelude::*;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

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
    replica: Replica,
    secret: u64,
    conn: Option<Conn>,
}

struct World {
    hub: Hub,
    clients: Vec<Client>,
    /// The server's document after each event, by sequence number.
    history: Vec<Project>,
    /// Everything the server sent to a connection that only listens.
    observer: Queue,
    /// A client has sent its pending edits again without knowing which the server already had.
    resent_blindly: bool,
}

fn sink(queue: &Queue) -> Sink {
    let queue = queue.clone();
    Box::new(move |msg| {
        queue.lock().unwrap().push_back(msg);
        true
    })
}

impl World {
    fn new(clients: u8) -> Self {
        let mut hub = Hub::open(Store::in_memory().unwrap()).unwrap();
        let history = vec![hub.doc().clone()];
        let observer = Queue::default();
        let hello = Hello {
            proto_version: PROTO_VERSION,
            token: hub.token().to_string(),
            user_key: [0xff; 16],
            name: "observer".into(),
            resume: None,
        };
        hub.join(hello, sink(&observer)).unwrap();
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
                    replica: Replica::new(actor, project, welcome.head_seq),
                    secret: welcome.session_secret,
                    conn: Some(Conn {
                        actor,
                        down,
                        up: VecDeque::new(),
                        welcomed: true,
                    }),
                }
            })
            .collect();
        Self {
            hub,
            clients,
            history,
            observer,
            resent_blindly: false,
        }
    }

    fn send(&mut self, client: usize, txn: Option<Txn>) {
        // Offline edits wait in the replica's pending queue.
        if let (Some(txn), Some(conn)) = (txn, &mut self.clients[client].conn)
            && conn.welcomed
        {
            conn.up.push_back(ClientMsg::Submit(txn));
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
                let resend = c
                    .replica
                    .resync(welcome.actor, welcome.head_seq, welcome.sync, welcome.acked)
                    .expect("the welcome continues where the replica is");
                conn.up.extend(resend.into_iter().map(ClientMsg::Submit));
                conn.welcomed = true;
                c.secret = welcome.session_secret;
            }
            ServerMsg::Event(event) => c.replica.on_event(&event).expect("events arrive in order"),
            ServerMsg::Rejected { client_txn_id, .. } => c.replica.on_rejected(client_txn_id),
            ServerMsg::Peers(_) | ServerMsg::Presence { .. } | ServerMsg::Transport { .. } => {}
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
        self.resent_blindly |= forget;
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

        // The log the observer saw is the log, and no edit is in it twice.
        let mut doc = self.history[0].clone();
        let mut seen = std::collections::HashSet::new();
        let mut events = 0;
        for msg in self.observer.lock().unwrap().iter() {
            let ServerMsg::Event(event) = msg else { continue };
            events += 1;
            assert_eq!(event.seq, events);
            apply_all(&mut doc, event.ops());
            let first = seen.insert((event.user, event.client_txn_id()));
            assert!(
                first || self.resent_blindly,
                "event {} repeats an edit already in the log",
                event.seq
            );
        }
        assert_eq!(events, self.hub.head());
        assert_eq!(&doc, self.hub.doc());
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

fn pick<T: Copy>(from: &[T], a: usize) -> Option<T> {
    (!from.is_empty()).then(|| from[a % from.len()])
}

/// An edit a user could make to `view`. Empty when the document has nothing to make it on.
fn edit(view: &Project, ids: &mut IdGen, kind: u8, a: usize, b: u8) -> Vec<Op> {
    let track = pick(&tracks(view), a);
    let clip = pick(&clips(view), a);
    let note = pick(&notes(view), a);
    let op = match kind % 14 {
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
                milli_bpm: 60_000 + u32::from(b) * 1000,
            },
        }),
        11 => track.map(|id| Op::TrackMove {
            id,
            after: pick(&tracks(view), usize::from(b)).filter(|&t| t != id),
        }),
        12 => Some(Op::ProjectSetName {
            name: format!("song {b}"),
        }),
        // The server refuses plugin devices outside a commit, and with them the whole edit.
        _ => {
            let device = Device::new(
                ids.device(),
                DeviceKind::Clap {
                    plugin_id: "com.example".into(),
                    state: None,
                },
            );
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
        2 => (client, prop::bool::weighted(0.15)).prop_map(|(client, forget)| Action::Connect { client, forget }),
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
