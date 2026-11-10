//! One editing session: the replica, the thing that orders its edits, and the audio engine.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use mdaw_dsp::pocket;
use mdaw_engine::Engine;
use mdaw_model::{
    ActorId, BuiltinKind, Device, DeviceKind, EntityRef, IdGen, Op, Project, TempoPoint, Tick, TrackContent,
    TrackId, TrackShell, UserId, apply, apply_all,
};
use mdaw_net::JoinInfo;
use mdaw_proto::{
    ClientMsg, Event, EventBody, Peer, Presence, ServerMsg, Superseded, SyncData, Txn, Welcome,
};
use mdaw_sync::{Gesture, Loopback, Replica};

use crate::link::{Link, LinkEvent, Waker, Who};
use crate::theme;

/// Actor that owns the IDs of the starter project, so they cannot collide with the user's.
const TEMPLATE_ACTOR: ActorId = ActorId(0);
const LOCAL_ACTOR: ActorId = ActorId(1);
const LOCAL_USER: UserId = UserId(1);

/// How long joining waits for the server.
const JOIN_TIMEOUT: Duration = Duration::from_secs(10);
/// Presence is sent at most this often.
const PRESENCE_MIN: Duration = Duration::from_millis(100);
/// While something is being edited, presence is repeated this often.
const PRESENCE_REFRESH: Duration = Duration::from_millis(500);
/// A soft lock not refreshed for this long is gone.
const PRESENCE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_DISPLACED: usize = 500;
const MAX_NOTICES: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// No server: the project lives in this process.
    Local,
    Online,
    /// The connection broke. Edits are kept and sent when it is back.
    Reconnecting,
    /// The server turned this client away for good.
    Refused,
}

/// An edit that lost against a concurrent one, for the changelog.
#[derive(Clone, Debug)]
pub struct Displaced {
    /// The event that displaced it.
    pub seq: u64,
    /// Who made the edit that won.
    pub by: UserId,
    pub lost: Superseded,
}

pub struct Session {
    replica: Replica,
    backend: Backend,
    engine: Engine,
    gesture_seq: u64,
    user: UserId,
    /// What this user is dragging right now, for the others to see.
    editing: Option<EntityRef>,
    displaced: Vec<Displaced>,
    notices: Vec<String>,
}

enum Backend {
    Local(Loopback),
    Remote(Box<Remote>),
}

struct Remote {
    link: Link,
    /// Identifies the current connection to the link.
    epoch: u64,
    status: Status,
    /// Set when the replica lost its place in the log, until the fresh join arrives.
    desynced: bool,
    peers: Vec<Peer>,
    /// Display names of everyone seen so far.
    names: HashMap<UserId, String>,
    /// What the others are doing, and when they last said so.
    presence: HashMap<ActorId, (Presence, Instant)>,
    presence_sent: Presence,
    presence_sent_at: Instant,
}

impl Session {
    /// A local session on `project`. Edits are ordered by an in-process loopback.
    pub fn local(project: Project, engine: Engine) -> Self {
        Self::new(
            Replica::new(LOCAL_ACTOR, project, 0),
            Backend::Local(Loopback::new(0, LOCAL_USER)),
            LOCAL_USER,
            engine,
        )
    }

    /// A local session on a new project with one instrument track.
    pub fn starter(engine: Engine) -> Self {
        let mut ids = IdGen::new(TEMPLATE_ACTOR);
        let mut project = Project::default();
        let tempo = TempoPoint {
            tick: Tick::ZERO,
            milli_bpm: mdaw_model::DEFAULT_MILLI_BPM,
        };
        apply_all(
            &mut project,
            &[
                Op::TempoPut {
                    id: ids.tempo(),
                    point: tempo,
                },
                new_track(&mut ids, 0, None),
            ],
        );
        Self::local(project, engine)
    }

    /// Joins the project a server holds. Waits until the project has arrived.
    ///
    /// `waker` is called from another thread whenever [`Session::frame`] has something to do.
    pub fn join(join: JoinInfo, who: Who, engine: Engine, waker: Waker) -> Result<Self> {
        let link = Link::connect(join, who, waker)?;
        let deadline = Instant::now() + JOIN_TIMEOUT;
        let (epoch, welcome) = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match link.wait(left) {
                Some(LinkEvent::Welcome { epoch, welcome }) => break (epoch, welcome),
                Some(LinkEvent::Refused(reason)) => bail!("{reason}"),
                // The link would keep trying, but a first attempt that fails is more likely a
                // wrong join string than a server about to come back.
                Some(LinkEvent::Down(reason)) => bail!("could not join: {reason}"),
                Some(LinkEvent::Msg(_)) => {}
                None => bail!("could not join: the server did not answer"),
            }
        };
        let Welcome {
            actor,
            user,
            head_seq,
            sync,
            peers,
            ..
        } = *welcome;
        let SyncData::Snapshot(project) = sync else {
            bail!("the server did not send the project");
        };
        let mut remote = Remote {
            link,
            epoch,
            status: Status::Online,
            desynced: false,
            peers: Vec::new(),
            names: HashMap::new(),
            presence: HashMap::new(),
            presence_sent: Presence::default(),
            presence_sent_at: Instant::now(),
        };
        remote.set_peers(peers);
        Ok(Self::new(
            Replica::new(actor, project, head_seq),
            Backend::Remote(Box::new(remote)),
            user,
            engine,
        ))
    }

    fn new(replica: Replica, backend: Backend, user: UserId, engine: Engine) -> Self {
        let mut session = Self {
            replica,
            backend,
            engine,
            gesture_seq: 0,
            user,
            editing: None,
            displaced: Vec::new(),
            notices: Vec::new(),
        };
        session.engine.set_project(session.replica.view());
        session
    }

    pub fn project(&self) -> &Project {
        self.replica.view()
    }

    pub fn ids(&mut self) -> &mut IdGen {
        self.replica.ids()
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }

    /// Applies one user gesture as one undo step. Returns false if it changed nothing.
    pub fn edit(&mut self, ops: Vec<Op>) -> bool {
        let txn = self.replica.submit(ops);
        self.send(txn)
    }

    /// Like [`Self::edit`], but edits with the same gesture collapse into one undo step. Use it
    /// for continuous controls, so the change is audible while the control moves.
    pub fn edit_gesture(&mut self, ops: Vec<Op>, gesture: Gesture) -> bool {
        let txn = self.replica.submit_gesture(ops, gesture);
        self.send(txn)
    }

    pub fn new_gesture(&mut self) -> Gesture {
        self.gesture_seq += 1;
        Gesture(self.gesture_seq)
    }

    pub fn can_undo(&self) -> bool {
        self.replica.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.replica.can_redo()
    }

    pub fn undo(&mut self) -> bool {
        let txn = self.replica.undo();
        self.send(txn)
    }

    pub fn redo(&mut self) -> bool {
        let txn = self.replica.redo();
        self.send(txn)
    }

    /// Call once per UI frame.
    pub fn frame(&mut self) {
        self.receive();
        self.share_presence();
        self.engine.set_project(self.replica.view());
        self.engine.tick();
    }

    pub fn status(&self) -> Status {
        match &self.backend {
            Backend::Local(_) => Status::Local,
            Backend::Remote(remote) => remote.status,
        }
    }

    pub fn user(&self) -> UserId {
        self.user
    }

    pub fn actor(&self) -> ActorId {
        self.replica.actor()
    }

    /// Everyone connected, this client included. Empty without a server.
    pub fn peers(&self) -> &[Peer] {
        match &self.backend {
            Backend::Local(_) => &[],
            Backend::Remote(remote) => &remote.peers,
        }
    }

    /// Own edits the server has not confirmed yet.
    pub fn unconfirmed(&self) -> usize {
        self.replica.pending().len()
    }

    /// Sequence number of the last event received.
    pub fn seq(&self) -> u64 {
        self.replica.seq()
    }

    /// How to call a user on screen.
    pub fn user_name(&self, user: UserId) -> String {
        if user == self.user {
            return "you".into();
        }
        let known = match &self.backend {
            Backend::Local(_) => None,
            Backend::Remote(remote) => remote.names.get(&user).cloned(),
        };
        known.unwrap_or_else(|| format!("user {}", user.0))
    }

    /// Edits that lost against concurrent ones, oldest first.
    pub fn displaced(&self) -> &[Displaced] {
        &self.displaced
    }

    /// Whether making a displaced edit again would change anything.
    pub fn can_restore(&self, lost: &Superseded) -> bool {
        apply(&mut self.replica.view().clone(), &lost.lost).is_some()
    }

    /// Makes a displaced edit again, as an ordinary edit of this user.
    pub fn restore(&mut self, lost: &Superseded) -> bool {
        self.edit(vec![lost.lost.clone()])
    }

    /// Things the user should know that happened behind their back, oldest first.
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    pub fn dismiss_notice(&mut self, index: usize) {
        if index < self.notices.len() {
            self.notices.remove(index);
        }
    }

    /// Tells the others what this user is dragging, so they leave it alone. Call every frame
    /// while it lasts, and with `None` after.
    pub fn set_editing(&mut self, editing: Option<EntityRef>) {
        self.editing = editing;
    }

    /// The name of another user who is busy with `entity` or with something it is part of.
    pub fn locked_by(&self, entity: &EntityRef) -> Option<&str> {
        let Backend::Remote(remote) = &self.backend else {
            return None;
        };
        let now = Instant::now();
        let (actor, _) = remote.presence.iter().find(|(_, (presence, at))| {
            now.duration_since(*at) < PRESENCE_TIMEOUT
                && presence.editing.is_some_and(|theirs| entity.is_within(&theirs))
        })?;
        let peer = remote.peers.iter().find(|peer| peer.actor == *actor)?;
        Some(peer.name.as_str())
    }

    /// When the screen has to be drawn again although nothing arrives: soft locks run out and
    /// presence wants repeating.
    pub fn next_frame_in(&self) -> Option<Duration> {
        let Backend::Remote(remote) = &self.backend else {
            return None;
        };
        let now = Instant::now();
        let locks = remote
            .presence
            .values()
            .any(|(presence, at)| presence.editing.is_some() && now.duration_since(*at) < PRESENCE_TIMEOUT);
        (locks || self.editing.is_some() || remote.presence_sent.editing.is_some())
            .then_some(Duration::from_millis(250))
    }

    fn send(&mut self, txn: Option<Txn>) -> bool {
        let Some(txn) = txn else { return false };
        match &mut self.backend {
            Backend::Local(server) => {
                let event = server.sequence(self.replica.actor(), txn);
                self.replica
                    .on_event(&event)
                    .expect("the loopback delivers events in order");
            }
            // While the connection is down the edit waits in the replica; it is sent with
            // everything else pending once the server welcomes us back.
            Backend::Remote(remote) => {
                if remote.status == Status::Online {
                    remote.link.send(remote.epoch, ClientMsg::Submit(txn));
                }
            }
        }
        self.engine.set_project(self.replica.view());
        true
    }

    /// Takes in what arrived from the server since the last frame.
    fn receive(&mut self) {
        let Backend::Remote(remote) = &mut self.backend else {
            return;
        };
        while let Some(event) = remote.link.poll() {
            match event {
                LinkEvent::Welcome { epoch, welcome } => {
                    let Welcome {
                        actor,
                        user,
                        head_seq,
                        sync,
                        acked,
                        peers,
                        ..
                    } = *welcome;
                    remote.epoch = epoch;
                    remote.set_peers(peers);
                    remote.presence_sent = Presence::default();
                    self.user = user;
                    match self.replica.resync(actor, head_seq, sync, acked) {
                        Ok(resend) => {
                            remote.status = Status::Online;
                            if remote.desynced {
                                remote.desynced = false;
                                // The undo history refers to a document that is gone.
                                notice(&mut self.notices, "Reloaded the project from the server.");
                            }
                            for txn in resend {
                                remote.link.send(epoch, ClientMsg::Submit(txn));
                            }
                        }
                        Err(_) => remote.lose_track(),
                    }
                }
                LinkEvent::Msg(_) if remote.desynced => {}
                LinkEvent::Msg(ServerMsg::Event(event)) => match self.replica.on_event(&event) {
                    Ok(()) => record_displaced(&mut self.displaced, event),
                    Err(_) => remote.lose_track(),
                },
                LinkEvent::Msg(ServerMsg::Rejected {
                    client_txn_id,
                    reason,
                }) => {
                    self.replica.on_rejected(client_txn_id);
                    notice(&mut self.notices, &format!("An edit was not accepted: {reason}."));
                }
                LinkEvent::Msg(ServerMsg::Peers(peers)) => remote.set_peers(peers),
                LinkEvent::Msg(ServerMsg::Presence { actor, presence }) => {
                    remote.presence.insert(actor, (presence, Instant::now()));
                }
                // Only ever the first answer on a connection, which the link deals with.
                LinkEvent::Msg(ServerMsg::Welcome(_) | ServerMsg::Refused(_)) => {}
                LinkEvent::Down(_) => {
                    if remote.status == Status::Online {
                        remote.status = Status::Reconnecting;
                    }
                    remote.presence.clear();
                }
                LinkEvent::Refused(reason) => {
                    remote.status = Status::Refused;
                    remote.presence.clear();
                    notice(&mut self.notices, &format!("Disconnected: {reason}."));
                }
            }
        }
    }

    fn share_presence(&mut self) {
        let Backend::Remote(remote) = &mut self.backend else {
            return;
        };
        if remote.status != Status::Online {
            return;
        }
        let presence = Presence {
            editing: self.editing,
            playhead: None,
        };
        let since = remote.presence_sent_at.elapsed();
        let due = if presence != remote.presence_sent {
            since >= PRESENCE_MIN
        } else {
            presence.editing.is_some() && since >= PRESENCE_REFRESH
        };
        if due {
            remote.link.send(remote.epoch, ClientMsg::Presence(presence));
            remote.presence_sent = presence;
            remote.presence_sent_at = Instant::now();
        }
    }
}

impl Remote {
    fn set_peers(&mut self, peers: Vec<Peer>) {
        for peer in &peers {
            self.names.insert(peer.user, peer.name.clone());
        }
        self.presence
            .retain(|actor, _| peers.iter().any(|peer| peer.actor == *actor));
        self.peers = peers;
    }

    /// The replica and the server's log no longer fit together: start over with a snapshot.
    fn lose_track(&mut self) {
        self.desynced = true;
        self.status = Status::Reconnecting;
        self.link.rejoin();
    }
}

fn notice(notices: &mut Vec<String>, text: &str) {
    if notices.last().is_some_and(|last| last == text) {
        return;
    }
    if notices.len() == MAX_NOTICES {
        notices.remove(0);
    }
    notices.push(text.to_string());
}

fn record_displaced(displaced: &mut Vec<Displaced>, event: Event) {
    let (seq, by) = (event.seq, event.user);
    let EventBody::Txn { superseded, .. } = event.body;
    displaced.extend(superseded.into_iter().map(|lost| Displaced { seq, by, lost }));
    if displaced.len() > MAX_DISPLACED {
        displaced.drain(..displaced.len() - MAX_DISPLACED);
    }
}

/// The op that adds an instrument track named after its position.
pub fn new_track(ids: &mut IdGen, index: usize, after: Option<TrackId>) -> Op {
    let mut content = TrackContent::default();
    let kind = DeviceKind::Builtin {
        kind: BuiltinKind::Pocket,
        dsp_version: pocket::DSP_VERSION,
    };
    content.devices.push_back(Device::new(ids.device(), kind));
    let shell = TrackShell {
        name: format!("Track {}", index + 1),
        color: theme::PALETTE[index % theme::PALETTE.len()],
    };
    Op::TrackAdd {
        id: ids.track(),
        shell,
        content,
        after,
    }
}
