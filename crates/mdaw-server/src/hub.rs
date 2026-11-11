//! One project and everyone connected to it. Runs on a single thread: whatever order commands
//! arrive in is the order of the log.

use crate::blobs::Blobs;
use crate::clock::Clock;
use crate::sequencer::Sequencer;
use crate::store::Run;
use crate::store::Store;
use anyhow::{Context, Result};
use mdaw_model::{ActorId, DEFAULT_MILLI_BPM, IdGen, Op, Project, TempoPoint, Tick, TrackId, UserId, apply};
use mdaw_proto::{
    ClientMsg, CommitRequest, Event, Hello, LEASE_STALE_SECS, PROTO_VERSION, Peer, Presence, RefuseReason,
    RejectReason, Request, Sample, ServerMsg, SyncData, TransportState, Txn, Welcome, apply_event,
};
use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

/// A snapshot of the document is stored every this many events, so opening a project does not
/// replay its whole history.
const SNAPSHOT_EVERY: u64 = 1000;

/// A client missing more events than this gets the whole document instead.
const CATCH_UP_LIMIT: u64 = 5000;

/// Changelog entries a new client gets with the document.
const HISTORY: usize = 200;
/// A catch-up of more bytes than this is sent as a snapshot instead.
const CATCH_UP_BYTES: usize = 64 << 20;
/// The history sent with a snapshot is cut to this many bytes, oldest first.
const HISTORY_BYTES: usize = 16 << 20;

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
    /// Every start of the server, oldest first.
    runs: Vec<Run>,
    clock: Clock,
    /// The shared transport. Lives as long as the process.
    transport: TransportState,
    blobs: Blobs,
    /// When the owner of each checkout last renewed its lease, on `clock`.
    leases: BTreeMap<TrackId, i64>,
    /// The checkouts last announced as stale.
    stale: Vec<TrackId>,
}

impl Hub {
    /// Loads the project from `store`, creating an empty one in a new store.
    pub fn open(store: Store, blobs: Blobs) -> Result<Self> {
        let token = match store.meta("token")? {
            Some(token) => String::from_utf8(token).context("stored token")?,
            None => {
                let token = hex(&random::<16>()?);
                store.set_meta_durably("token", token.as_bytes())?;
                token
            }
        };
        let key = match store.meta("session_key")? {
            Some(key) => key.as_slice().try_into().ok().context("stored session key")?,
            None => {
                let key = random::<32>()?;
                store.set_meta_durably("session_key", &key)?;
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
                apply_event(&mut doc, event);
            }
            at = last;
        }
        let recent = store.events_after(head.saturating_sub(4096), 4096)?;
        let runs = store.start_run(head)?;
        let clock = Clock::new();
        // After a restart every owner gets a full lease to come back.
        let leases = doc
            .tracks
            .iter()
            .filter(|(_, t)| t.checkout.is_some())
            .map(|(&id, _)| (id, clock.micros()))
            .collect();
        Ok(Self {
            store,
            seq: Sequencer::new(doc, head, recent),
            token,
            key,
            sessions: BTreeMap::new(),
            departed: HashMap::new(),
            snapshot_seq,
            clock,
            transport: TransportState::default(),
            blobs,
            leases,
            stale: Vec::new(),
            runs,
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

    pub fn blobs(&self) -> &Blobs {
        &self.blobs
    }

    /// The checkouts whose lease ran out, as last announced.
    pub fn stale(&self) -> &[TrackId] {
        &self.stale
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

        let resume = hello.resume.filter(|r| {
            // A client of an earlier run claims to have events that a later start did not find.
            let lost = self
                .runs
                .iter()
                .any(|run| r.prev_actor <= run.last_actor && r.last_seq > run.head);
            r.session_secret == self.secret(r.prev_actor) && r.last_seq <= head && !lost
        });
        let mut history = Vec::new();
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
        if matches!(sync, SyncData::Snapshot(_)) {
            history = self.seq.history(HISTORY);
            let mut room = HISTORY_BYTES;
            let keep = history
                .iter()
                .rev()
                .take_while(|e| match room.checked_sub(encoded_len(e)) {
                    Some(left) => {
                        room = left;
                        true
                    }
                    None => false,
                })
                .count();
            history.drain(..history.len() - keep);
        }

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
            history,
            stale: self.stale.clone(),
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
            ClientMsg::Checkout { track } => self.checkout(actor, track),
            ClientMsg::LeaseRenew { track } => self.renew(actor, track),
            ClientMsg::Release { track } => self.release(actor, track, false),
            ClientMsg::ForceRelease { track } => self.release(actor, track, true),
            ClientMsg::Commit(commit) => self.commit(actor, *commit),
            ClientMsg::Attach { entry_seq, sample } => self.attach(actor, entry_seq, sample),
        }
    }

    pub fn checkout(&mut self, actor: ActorId, track: TrackId) {
        let Some(user) = self.sessions.get(&actor).map(|s| s.user) else {
            return;
        };
        let result = self.seq.checkout(actor, user, now_millis(), track);
        if result.is_ok() {
            self.leases.insert(track, self.clock.micros());
        }
        self.settle(actor, Request::Checkout(track), result);
    }

    pub fn release(&mut self, actor: ActorId, track: TrackId, forced: bool) {
        let Some(user) = self.sessions.get(&actor).map(|s| s.user) else {
            return;
        };
        let result = self.seq.release(actor, user, now_millis(), track, forced);
        let request = if forced {
            Request::ForceRelease(track)
        } else {
            Request::Release(track)
        };
        self.settle(actor, request, result);
    }

    pub fn commit(&mut self, actor: ActorId, commit: CommitRequest) {
        let Some(user) = self.sessions.get(&actor).map(|s| s.user) else {
            return;
        };
        let request = Request::Commit(commit.track);
        let now = self.clock.micros();
        let tempo = self.transport.running(now).then(|| self.seq.doc().tempo_map());
        let blobs = &self.blobs;
        let result = self
            .seq
            .commit(actor, user, now_millis(), commit, |h| blobs.has(h));
        let accepted = result.is_ok();
        self.settle(actor, request, result);
        if accepted {
            self.follow_tempo(now, tempo);
        }
    }

    pub fn attach(&mut self, actor: ActorId, entry_seq: u64, sample: Sample) {
        let Some(user) = self.sessions.get(&actor).map(|s| s.user) else {
            return;
        };
        let blobs = &self.blobs;
        let result = self
            .seq
            .attach(actor, user, now_millis(), entry_seq, sample, |h| blobs.has(h));
        self.settle(actor, Request::Attach(entry_seq), result);
    }

    /// The owner of a checkout is still at it.
    pub fn renew(&mut self, actor: ActorId, track: TrackId) {
        let Some(user) = self.sessions.get(&actor).map(|s| s.user) else {
            return;
        };
        let owned = self
            .seq
            .doc()
            .track(track)
            .and_then(|t| t.checkout)
            .is_some_and(|c| c.user == user);
        if owned {
            self.leases.insert(track, self.clock.micros());
            self.check_leases();
        }
    }

    /// Looks at the time. Call about once a second.
    pub fn tick(&mut self) {
        self.check_leases();
    }

    /// Announces the checkouts whose lease ran out, when that set changes.
    fn check_leases(&mut self) {
        let now = self.clock.micros();
        let limit = LEASE_STALE_SECS as i64 * 1_000_000;
        let doc = self.seq.doc();
        self.leases
            .retain(|id, _| doc.track(*id).is_some_and(|t| t.checkout.is_some()));
        let stale: Vec<TrackId> = self
            .leases
            .iter()
            .filter(|(_, at)| now - **at > limit)
            .map(|(id, _)| *id)
            .collect();
        if stale != self.stale {
            self.stale = stale.clone();
            self.broadcast(None, &ServerMsg::Stale(stale));
        }
    }

    /// Stores and sends an accepted event, or tells the asker why not.
    fn settle(&mut self, actor: ActorId, request: Request, result: Result<Event, RejectReason>) {
        match result {
            Ok(event) => {
                self.record(event);
                self.check_leases();
            }
            Err(reason) => {
                let Some(session) = self.sessions.get_mut(&actor) else {
                    return;
                };
                if !(session.sink)(ServerMsg::Denied { request, reason }) {
                    self.leave(actor);
                }
            }
        }
    }

    /// Puts a sequenced event on disk and sends it to everyone.
    fn record(&mut self, event: Event) {
        // A log the disk refuses cannot be continued in memory: clients would hold events that
        // are gone after a restart.
        self.store.append(&event).expect("appending to the event log");
        if event.seq - self.snapshot_seq >= SNAPSHOT_EVERY {
            self.store
                .put_snapshot(event.seq, self.seq.doc())
                .expect("storing a snapshot");
            self.snapshot_seq = event.seq;
        }
        self.broadcast(None, &ServerMsg::Event(event));
    }

    /// After an event: if the tempo changed under a running transport, keeps the playhead
    /// where it was. `old` is the tempo map before the event, taken only while running.
    fn follow_tempo(&mut self, now: i64, old: Option<mdaw_model::TempoMap>) {
        if let Some(old) = old.filter(|old| *old != self.seq.doc().tempo_map()) {
            // Everyone was where the old tempo put them. Going on from there keeps the music
            // continuous; the old anchor under the new tempo would be a jump.
            self.transport = self.transport.reanchored(now, &old);
            self.transport.epoch += 1;
            let state = self.transport;
            self.broadcast(None, &ServerMsg::Transport { state, by: None });
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
                self.record(event);
                self.follow_tempo(now, tempo);
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

    /// The user behind a connected actor. A made-up one for an actor that is gone.
    pub fn user_of(&self, actor: ActorId) -> UserId {
        self.sessions.get(&actor).map_or(UserId(0), |s| s.user)
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
        // Large commits make few events many bytes; past the budget a snapshot is smaller.
        let bytes: usize = events.iter().map(encoded_len).sum();
        if bytes > CATCH_UP_BYTES {
            return SyncData::Snapshot(self.seq.doc().clone());
        }
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

fn encoded_len(event: &Event) -> usize {
    postcard::to_stdvec(event).map_or(usize::MAX, |bytes| bytes.len())
}
