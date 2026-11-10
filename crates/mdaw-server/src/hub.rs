//! One project and everyone connected to it. Runs on a single thread: whatever order commands
//! arrive in is the order of the log.

use crate::clock::Clock;
use crate::sequencer::Sequencer;
use crate::store::Store;
use anyhow::{Context, Result};
use mdaw_model::{
    ActorId, DEFAULT_MILLI_BPM, IdGen, Op, Project, TempoPoint, Tick, UserId, apply, apply_all,
};
use mdaw_proto::{
    ClientMsg, Hello, PROTO_VERSION, Peer, Presence, RefuseReason, ServerMsg, SyncData, TransportState, Txn,
    Welcome,
};
use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

/// A snapshot of the document is stored every this many events, so opening a project does not
/// replay its whole history.
const SNAPSHOT_EVERY: u64 = 1000;

/// A client missing more events than this gets the whole document instead.
const CATCH_UP_LIMIT: u64 = 5000;

/// IDs in the document a new project starts with belong to this actor, which no connection gets.
const TEMPLATE_ACTOR: ActorId = ActorId(0);

/// Delivers a message to one client. Returns false when the client cannot take it, be it gone
/// or too slow; the hub then drops the client, which may reconnect and resume.
pub type Sink = Box<dyn FnMut(ServerMsg) -> bool + Send>;

struct Session {
    user: UserId,
    name: String,
    sink: Sink,
    /// The highest transaction ID of this connection sequenced or rejected so far.
    last_txn: Option<u64>,
}

pub struct Hub {
    store: Store,
    seq: Sequencer,
    token: String,
    key: [u8; 32],
    sessions: BTreeMap<ActorId, Session>,
    /// `last_txn` of connections that have ended, for when their clients resume.
    departed: HashMap<ActorId, Option<u64>>,
    snapshot_seq: u64,
    clock: Clock,
    /// The shared transport. Lives as long as the process.
    transport: TransportState,
}

impl Hub {
    /// Loads the project from `store`, creating an empty one in a new store.
    pub fn open(store: Store) -> Result<Self> {
        let token = match store.meta("token")? {
            Some(token) => String::from_utf8(token).context("stored token")?,
            None => {
                let token = hex(&random::<16>()?);
                store.set_meta("token", token.as_bytes())?;
                token
            }
        };
        let key = match store.meta("session_key")? {
            Some(key) => key.as_slice().try_into().ok().context("stored session key")?,
            None => {
                let key = random::<32>()?;
                store.set_meta("session_key", &key)?;
                key
            }
        };
        let (snapshot_seq, mut doc) = match store.latest_snapshot()? {
            Some(snapshot) => snapshot,
            None => {
                let doc = new_project();
                store.put_snapshot(0, &doc)?;
                (0, doc)
            }
        };
        let head = store.head()?;
        let mut at = snapshot_seq;
        while at < head {
            let events = store.events_after(at, 1000)?;
            let last = events.last().context("the event log has a gap")?.seq;
            for event in &events {
                apply_all(&mut doc, event.ops());
            }
            at = last;
        }
        let recent = store.events_after(head.saturating_sub(4096), 4096)?;
        Ok(Self {
            store,
            seq: Sequencer::new(doc, head, recent),
            token,
            key,
            sessions: BTreeMap::new(),
            departed: HashMap::new(),
            snapshot_seq,
            clock: Clock::new(),
            transport: TransportState::default(),
        })
    }

    /// The secret clients need to join.
    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn doc(&self) -> &Project {
        self.seq.doc()
    }

    pub fn head(&self) -> u64 {
        self.seq.head()
    }

    /// The clock the transport's anchors are measured on.
    pub fn clock(&self) -> Clock {
        self.clock
    }

    pub fn transport(&self) -> &TransportState {
        &self.transport
    }

    /// Admits a client. Its `Welcome` is the first thing `sink` receives; from then on it gets
    /// every event.
    pub fn join(&mut self, hello: Hello, mut sink: Sink) -> Result<ActorId, RefuseReason> {
        if hello.proto_version != PROTO_VERSION {
            return Err(RefuseReason::ProtoVersion(PROTO_VERSION));
        }
        if !same(hello.token.as_bytes(), self.token.as_bytes()) {
            return Err(RefuseReason::BadToken);
        }
        let name = display_name(&hello.name);
        let user = self.store.user(&hello.user_key, &name).expect("storing the user");
        let actor = self.store.next_actor().expect("storing the actor counter");
        let head = self.seq.head();

        let resume = hello
            .resume
            .filter(|r| r.session_secret == self.secret(r.prev_actor) && r.last_seq <= head);
        let (sync, acked) = match resume {
            Some(resume) => {
                // Whatever connection the client had before is replaced by this one.
                self.remove(resume.prev_actor);
                let acked = match self.departed.get(&resume.prev_actor) {
                    Some(last) => *last,
                    // The server restarted in between. What it rejected back then is forgotten
                    // and comes in again, to be rejected again.
                    None => self
                        .store
                        .last_txn(resume.prev_actor)
                        .expect("reading the event log"),
                };
                (self.catch_up(resume.last_seq), acked)
            }
            None => (SyncData::Snapshot(self.seq.doc().clone()), None),
        };

        self.sessions.insert(
            actor,
            Session {
                user,
                name,
                sink: Box::new(|_| true),
                last_txn: None,
            },
        );
        let welcome = Welcome {
            actor,
            user,
            session_secret: self.secret(actor),
            head_seq: head,
            sync,
            acked,
            peers: self.peers(),
            transport: self.transport,
        };
        if !sink(ServerMsg::Welcome(Box::new(welcome))) {
            self.sessions.remove(&actor);
            return Ok(actor);
        }
        self.sessions.get_mut(&actor).expect("just inserted").sink = sink;
        self.broadcast_peers();
        Ok(actor)
    }

    /// Handles a message from a connected client. Messages of unknown actors are dropped: the
    /// connection has been replaced or was too slow.
    pub fn handle(&mut self, actor: ActorId, msg: ClientMsg) {
        match msg {
            ClientMsg::Hello(_) => self.leave(actor),
            ClientMsg::Submit(txn) => self.submit(actor, txn),
            ClientMsg::Presence(presence) => self.presence(actor, presence),
            ClientMsg::SetTransport(state) => self.set_transport(actor, state),
        }
    }

    pub fn submit(&mut self, actor: ActorId, txn: Txn) {
        let Some(session) = self.sessions.get_mut(&actor) else {
            return;
        };
        let client_txn_id = txn.client_txn_id;
        session.last_txn = session.last_txn.max(Some(client_txn_id));
        let now = self.clock.micros();
        // While the transport runs, a change of tempo changes where the playhead is.
        let tempo = self.transport.running(now).then(|| self.seq.doc().tempo_map());
        match self.seq.sequence(actor, session.user, now_millis(), txn) {
            Ok(event) => {
                // A log the disk refuses cannot be continued in memory: clients would hold
                // events that are gone after a restart.
                self.store.append(&event).expect("appending to the event log");
                if event.seq - self.snapshot_seq >= SNAPSHOT_EVERY {
                    self.store
                        .put_snapshot(event.seq, self.seq.doc())
                        .expect("storing a snapshot");
                    self.snapshot_seq = event.seq;
                }
                self.broadcast(None, &ServerMsg::Event(event));
                if let Some(old) = tempo.filter(|old| *old != self.seq.doc().tempo_map()) {
                    // Everyone was where the old tempo put them. Going on from there keeps the
                    // music continuous; the old anchor under the new tempo would be a jump.
                    self.transport = self.transport.reanchored(now, &old);
                    self.transport.epoch += 1;
                    let state = self.transport;
                    self.broadcast(None, &ServerMsg::Transport { state, by: None });
                }
            }
            Err(reason) => {
                if !(session.sink)(ServerMsg::Rejected {
                    client_txn_id,
                    reason,
                }) {
                    self.leave(actor);
                }
            }
        }
    }

    pub fn presence(&mut self, actor: ActorId, presence: Presence) {
        if self.sessions.contains_key(&actor) {
            self.broadcast(Some(actor), &ServerMsg::Presence { actor, presence });
        }
    }

    /// Any client may move the shared transport; the last request wins. Everyone is told,
    /// the one who asked too: the state counts from the server's answer on.
    pub fn set_transport(&mut self, actor: ActorId, wanted: TransportState) {
        if !self.sessions.contains_key(&actor) {
            return;
        }
        let by = Some(actor);
        match wanted.accepted(self.clock.micros(), &self.transport) {
            Some(state) => {
                self.transport = state;
                self.broadcast(None, &ServerMsg::Transport { state, by });
            }
            None => {
                // The client already acts on what it asked for: put it right.
                let state = self.transport;
                let current = ServerMsg::Transport { state, by };
                let session = self.sessions.get_mut(&actor).expect("checked above");
                if !(session.sink)(current) {
                    self.leave(actor);
                }
            }
        }
    }

    /// The client's connection ended.
    pub fn leave(&mut self, actor: ActorId) {
        if self.remove(actor) {
            self.broadcast_peers();
        }
    }

    pub fn peers(&self) -> Vec<Peer> {
        self.sessions
            .iter()
            .map(|(&actor, s)| Peer {
                actor,
                user: s.user,
                name: s.name.clone(),
            })
            .collect()
    }

    fn remove(&mut self, actor: ActorId) -> bool {
        let Some(session) = self.sessions.remove(&actor) else {
            return false;
        };
        if self.departed.len() >= 4096 {
            self.departed.clear();
        }
        self.departed.insert(actor, session.last_txn);
        if self.sessions.is_empty() && self.transport.playing {
            // Nobody listens: whoever comes next finds the playhead where it was left.
            let now = self.clock.micros();
            let tick = self.transport.position(now, &self.seq.doc().tempo_map());
            self.transport = TransportState {
                playing: false,
                anchor_tick: tick.round(),
                anchor_server_micros: now,
                loop_range: self.transport.loop_range,
                epoch: self.transport.epoch + 1,
            };
        }
        true
    }

    fn catch_up(&self, last_seq: u64) -> SyncData {
        if self.seq.head() - last_seq > CATCH_UP_LIMIT {
            return SyncData::Snapshot(self.seq.doc().clone());
        }
        let events = match self.seq.events_after(last_seq) {
            Some(events) => events,
            None => self
                .store
                .events_after(last_seq, CATCH_UP_LIMIT)
                .expect("reading the event log"),
        };
        SyncData::Events(events)
    }

    /// Sends to everyone but `except`, dropping clients that cannot keep up.
    fn broadcast(&mut self, except: Option<ActorId>, msg: &ServerMsg) {
        let mut gone = Vec::new();
        for (&actor, session) in &mut self.sessions {
            if Some(actor) != except && !(session.sink)(msg.clone()) {
                gone.push(actor);
            }
        }
        if !gone.is_empty() {
            for actor in gone {
                self.remove(actor);
            }
            self.broadcast_peers();
        }
    }

    fn broadcast_peers(&mut self) {
        let peers = ServerMsg::Peers(self.peers());
        self.broadcast(None, &peers);
    }

    /// The session secret of `actor`: derived, so it survives a restart without being stored.
    fn secret(&self, actor: ActorId) -> u64 {
        let hash = blake3::keyed_hash(&self.key, &actor.0.to_le_bytes());
        u64::from_le_bytes(hash.as_bytes()[..8].try_into().expect("eight bytes"))
    }
}

/// The document of a project nobody has edited yet.
fn new_project() -> Project {
    let mut project = Project::default();
    let point = TempoPoint {
        tick: Tick::ZERO,
        milli_bpm: DEFAULT_MILLI_BPM,
    };
    let id = IdGen::new(TEMPLATE_ACTOR).tempo();
    apply(&mut project, &Op::TempoPut { id, point });
    project
}

fn display_name(name: &str) -> String {
    let name: String = name.trim().chars().filter(|c| !c.is_control()).take(32).collect();
    if name.is_empty() { "anonymous".into() } else { name }
}

fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no random numbers from the system: {e}"))?;
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Comparison that takes the same time wherever the inputs differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
