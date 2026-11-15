//! One project and everyone connected to it. Runs on a single thread: whatever order commands
//! arrive in is the order of the log.
//!
//! A transaction is decided, then written to disk, and only then applied and told to anyone
//! (see [`Hub::submit`]): no client ever hears of an event the disk does not have.

use crate::blobs::Blobs;
use crate::sequencer::Sequencer;
use crate::store::{Receipt, Run, Store};
use anyhow::{Context, Result};
use mdaw_model::{ActorId, DEFAULT_MILLI_BPM, IdGen, Op, Project, TempoPoint, Tick, UserId, apply};
use mdaw_proto::{
    ClientMsg, Event, Hello, PROTO_VERSION, Peer, Presence, RefuseReason, ServerMsg, SyncData, Txn, Welcome,
    apply_event,
};
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// A snapshot of the document is stored every this many events, so opening a project does not
/// replay its whole history.
const SNAPSHOT_EVERY: u64 = 1000;

/// A client missing more events than this gets the whole document instead.
const CATCH_UP_LIMIT: u64 = 5000;

/// History entries a new client gets with the document.
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
}

pub struct Hub {
    store: Store,
    seq: Sequencer,
    token: String,
    key: [u8; 32],
    sessions: BTreeMap<ActorId, Session>,
    snapshot_seq: u64,
    /// A client missing more events than this gets the whole document instead.
    catch_up_limit: u64,
    /// Every start of the server, oldest first.
    runs: Vec<Run>,
    blobs: Blobs,
}

impl Hub {
    /// Loads the project from `store`, creating an empty one in a new store.
    pub fn open(store: Store, blobs: Blobs) -> Result<Self> {
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
                apply_event(&mut doc, event);
            }
            at = last;
        }
        let recent = store.events_after(head.saturating_sub(4096), 4096)?;
        let runs = store.start_run(head)?;
        Ok(Self {
            store,
            seq: Sequencer::new(doc, head, recent),
            token,
            key,
            sessions: BTreeMap::new(),
            snapshot_seq,
            catch_up_limit: CATCH_UP_LIMIT,
            blobs,
            runs,
        })
    }

    /// Sends a client missing more than `limit` events the whole document instead. For tests
    /// of the snapshot path, which otherwise takes thousands of events to reach.
    pub fn with_catch_up_limit(mut self, limit: u64) -> Self {
        self.catch_up_limit = limit;
        self
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

    pub fn blobs(&self) -> &Blobs {
        &self.blobs
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
        let sync = match resume {
            Some(resume) => {
                // Whatever connection the client had before is replaced by this one, so it
                // leaves the peers at once. What it sent and was not answered comes again on
                // this one; receipts make that safe.
                self.remove(resume.prev_actor);
                self.catch_up(resume.last_seq)
            }
            None => SyncData::Snapshot(self.seq.doc().clone()),
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
            },
        );
        let welcome = Welcome {
            actor,
            user,
            session_secret: self.secret(actor),
            head_seq: head,
            sync,
            peers: self.peers(),
            history,
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
        }
    }

    /// Takes a transaction from a client. Exactly one of these happens:
    ///
    /// - The sender's user sent this transaction before: the sender alone gets the answer it got
    ///   then, `Accepted` with the event's seq or the same `Rejected`. Nothing else happens.
    /// - It is refused: the refusal is stored as its receipt, then the sender gets `Rejected`.
    /// - It is taken: its event and its receipt are committed to disk together. Only once the
    ///   commit has returned is the event applied to the document in memory and sent to
    ///   everyone, the sender included.
    ///
    /// If the disk refuses a write, nothing of the transaction is applied or sent, and the
    /// sender's connection is dropped; it will send the transaction again when it is back.
    pub fn submit(&mut self, actor: ActorId, txn: Txn) {
        let Some(session) = self.sessions.get(&actor) else {
            return;
        };
        let (user, id) = (session.user, txn.id);
        let receipt = match self.store.receipt(user, id) {
            Ok(receipt) => receipt,
            Err(err) => return self.fail(actor, err),
        };
        let answer = match receipt {
            Some(Receipt::Accepted(seq)) => ServerMsg::Accepted { txn: id, seq },
            Some(Receipt::Rejected(reason)) => ServerMsg::Rejected { txn: id, reason },
            None => {
                let blobs = &self.blobs;
                match self.seq.prepare(actor, user, now_millis(), txn, |h| blobs.has(h)) {
                    Ok(prepared) => {
                        // Decided; now on disk, event and receipt in one commit.
                        if let Err(err) = self.store.append(user, prepared.event()) {
                            // Dropping `prepared` leaves the document and the log in memory as
                            // they are on disk.
                            return self.fail(actor, err);
                        }
                        // On disk: from here on it may be applied and told.
                        let event = self.seq.commit(prepared);
                        self.snapshot_if_due();
                        self.broadcast(None, &ServerMsg::Event(event));
                        return;
                    }
                    Err(reason) => {
                        if let Err(err) = self.store.reject(user, id, &reason) {
                            return self.fail(actor, err);
                        }
                        ServerMsg::Rejected { txn: id, reason }
                    }
                }
            }
        };
        let session = self.sessions.get_mut(&actor).expect("looked up above");
        if !(session.sink)(answer) {
            self.leave(actor);
        }
    }

    /// The disk refused a write for `actor`'s transaction. Nothing was applied or sent; the
    /// client loses its connection, comes back, and sends what it has pending again.
    fn fail(&mut self, actor: ActorId, err: anyhow::Error) {
        eprintln!("mdaw-server: an edit could not be stored, dropping its sender ({actor:?}): {err:#}");
        self.leave(actor);
    }

    /// Stores the document now and then, so opening the project does not replay the whole log.
    /// A snapshot is only a shortcut: the events it sums up are on disk already, so one that
    /// cannot be written is skipped.
    fn snapshot_if_due(&mut self) {
        let head = self.seq.head();
        if head - self.snapshot_seq < SNAPSHOT_EVERY {
            return;
        }
        match self.store.put_snapshot(head, self.seq.doc()) {
            Ok(()) => self.snapshot_seq = head,
            Err(err) => eprintln!("mdaw-server: could not store a snapshot at event {head}: {err:#}"),
        }
    }

    pub fn presence(&mut self, actor: ActorId, presence: Presence) {
        if self.sessions.contains_key(&actor) {
            self.broadcast(Some(actor), &ServerMsg::Presence { actor, presence });
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
        self.sessions.remove(&actor).is_some()
    }

    fn catch_up(&self, last_seq: u64) -> SyncData {
        if self.seq.head() - last_seq > self.catch_up_limit {
            return SyncData::Snapshot(self.seq.doc().clone());
        }
        let events = match self.seq.events_after(last_seq) {
            Some(events) => events,
            None => self
                .store
                .events_after(last_seq, self.catch_up_limit)
                .expect("reading the event log"),
        };
        // Large edits make few events many bytes; past the budget a snapshot is smaller.
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
