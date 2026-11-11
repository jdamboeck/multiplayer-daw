//! One editing session: the replica, the thing that orders its edits, and the audio engine.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use mdaw_dsp::pocket;
use mdaw_engine::{Engine, clock, flac};
use mdaw_model::{
    ActorId, BlobHash, BuiltinKind, Device, DeviceKind, EntityRef, IdGen, Op, Project, Scope, TempoMap,
    TempoPoint, Tick, TickRange, TrackContent, TrackId, TrackShell, UserId, apply, apply_all,
};
use mdaw_net::{ClockEstimate, JoinInfo};
use mdaw_proto::{
    ChangelogEntry, ClientMsg, CommitRequest, Event, EventBody, LEASE_RENEW_SECS, Peer, Presence,
    ReleaseReason, Request, START_LEAD_MICROS, Sample, ServerMsg, Superseded, SyncData, TransportState, Txn,
    Welcome,
};
use mdaw_sync::{Ended, Gesture, Loopback, Replica, Workspace};

use crate::checkout::{self, CommitPlan, Files, Fork, Rendered};
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
const MAX_LOG: usize = 500;
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
    /// Server wall clock of that event, milliseconds since the Unix epoch.
    pub wall_ts: u64,
    pub lost: Superseded,
}

/// A checkout, commit or release, for the changelog.
#[derive(Clone, Debug)]
pub struct LogEntry {
    pub seq: u64,
    /// Who did it.
    pub by: UserId,
    /// Server wall clock, milliseconds since the Unix epoch.
    pub wall_ts: u64,
    pub what: Logged,
}

impl Logged {
    pub fn track(&self) -> TrackId {
        match self {
            Logged::Checkout { track } | Logged::Commit { track, .. } | Logged::Release { track, .. } => {
                *track
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum Logged {
    Checkout {
        track: TrackId,
    },
    Commit {
        track: TrackId,
        summary: String,
        ranges: Vec<TickRange>,
    },
    Release {
        track: TrackId,
        reason: ReleaseReason,
    },
}

/// Where a commit of this client stands.
pub enum CommitState<'a> {
    /// The samples are being rendered.
    Rendering { track: TrackId },
    /// Waiting for the user to decide.
    Ready { plan: &'a CommitPlan },
    /// On its way to the server.
    Sending { track: TrackId },
}

struct Commit {
    track: TrackId,
    plan: Option<CommitPlan>,
    /// The message, once the user has decided. Sent again on a new connection until the
    /// commit is in the log or turned down.
    sent: Option<ClientMsg>,
}

/// Results of work done on other threads.
enum Job {
    Plan(CommitPlan),
    Before { entry_seq: u64, rendered: Rendered },
    Nothing,
}

/// How well this client keeps time with the shared transport.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// The server's clock now, as this client estimates it.
    pub server_micros: i64,
    /// Round trip of the ping the estimate rests on. The estimate is off by at most half of
    /// it.
    pub rtt_micros: i64,
    /// How far the audio is ahead of where the shared transport puts it. `None` unless
    /// playing.
    pub drift_micros: Option<i64>,
    /// How often the engine had to jump to get back in step.
    pub resyncs: u64,
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
    /// The tempo map of the view when the transport was last looked at.
    tempo: TempoMap,
    /// The loop of the private transport.
    private_loop: Option<TickRange>,
    waker: Option<Waker>,
    /// Where checkouts and forks of this project are kept. `None`: nowhere.
    files: Option<Files>,
    /// Journal lengths as last written, by track. A workspace missing here is new to the
    /// session: a journal from before may wait for it on disk.
    saved: HashMap<TrackId, usize>,
    commit: Option<Commit>,
    jobs: (std_mpsc::Sender<Job>, std_mpsc::Receiver<Job>),
    /// Blobs not yet known to be on the server, with their bytes.
    uploads: HashMap<BlobHash, Arc<[u8]>>,
    /// Messages that go out once none of their blobs is still uploading.
    outgoing: Vec<(Vec<BlobHash>, ClientMsg)>,
    log: Vec<LogEntry>,
    /// Samples of changelog entries, by the entry's sequence number.
    samples: HashMap<u64, Vec<Sample>>,
    /// Decoded samples at the engine's rate.
    audio: HashMap<BlobHash, Arc<[f32]>>,
    /// A sample to play as soon as it has been downloaded.
    wanted: Option<BlobHash>,
    /// The sample playing, with its preview id.
    playing: Option<(BlobHash, u64)>,
    forks: Vec<Fork>,
    lease_sent: Instant,
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
    /// Whether this client takes part in the shared transport, as far as the user decides.
    linked: bool,
    /// The server's clock against the engine's, once the connection has measured it.
    clock: Option<ClockEstimate>,
    /// The shared transport: what the server said last, or what this client asked for since.
    transport: TransportState,
    /// The server spoke about the transport since it was last looked at.
    transport_heard: bool,
    /// Requests for the shared transport the server has not answered yet. While there are
    /// any, what it says is older than what this client asked for.
    transport_asked: u32,
    /// What the engine was last told to follow, and under which clock offset. `None` while
    /// the engine runs the private transport.
    applied: Option<(TransportState, i64)>,
    /// Checkouts whose owner has not renewed the lease in time.
    stale: Vec<TrackId>,
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
        let link = Link::connect(join, who, waker.clone())?;
        let deadline = Instant::now() + JOIN_TIMEOUT;
        let (epoch, welcome) = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match link.wait(left) {
                Some(LinkEvent::Welcome { epoch, welcome }) => break (epoch, welcome),
                Some(LinkEvent::Refused(reason)) => bail!("{reason}"),
                // The link would keep trying, but a first attempt that fails is more likely a
                // wrong join string than a server about to come back.
                Some(LinkEvent::Down(reason)) => bail!("could not join: {reason}"),
                Some(
                    LinkEvent::Msg(_)
                    | LinkEvent::Clock(_)
                    | LinkEvent::Stored { .. }
                    | LinkEvent::Fetched { .. },
                ) => {}
                None => bail!("could not join: the server did not answer"),
            }
        };
        let Welcome {
            actor,
            user,
            head_seq,
            sync,
            peers,
            transport,
            history,
            stale,
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
            linked: true,
            clock: None,
            transport,
            transport_heard: true,
            transport_asked: 0,
            applied: None,
            stale,
        };
        remote.set_peers(peers);
        let mut session = Self::new(
            Replica::new(actor, project, head_seq),
            Backend::Remote(Box::new(remote)),
            user,
            engine,
        );
        session.waker = Some(waker);
        session.absorb_history(history);
        Ok(session)
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
            tempo: TempoMap::default(),
            private_loop: None,
            waker: None,
            files: None,
            saved: HashMap::new(),
            commit: None,
            jobs: std_mpsc::channel(),
            uploads: HashMap::new(),
            outgoing: Vec::new(),
            log: Vec::new(),
            samples: HashMap::new(),
            audio: HashMap::new(),
            wanted: None,
            playing: None,
            forks: Vec::new(),
            lease_sent: Instant::now(),
        };
        session.replica.set_user(user);
        session.tempo = session.replica.view().tempo_map();
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
        if !self.allowed(&ops) {
            return false;
        }
        let before = self.replica.revision();
        let txn = self.replica.submit(ops);
        self.after_edit(txn, before)
    }

    /// Like [`Self::edit`], but edits with the same gesture collapse into one undo step. Use it
    /// for continuous controls, so the change is audible while the control moves.
    pub fn edit_gesture(&mut self, ops: Vec<Op>, gesture: Gesture) -> bool {
        if !self.allowed(&ops) {
            return false;
        }
        let before = self.replica.revision();
        let txn = self.replica.submit_gesture(ops, gesture);
        self.after_edit(txn, before)
    }

    /// An edit that only went into a checkout's journal sends nothing, but changed the view.
    fn after_edit(&mut self, txn: Option<Txn>, before: u64) -> bool {
        if txn.is_some() {
            return self.send(txn);
        }
        let changed = self.replica.revision() != before;
        if changed {
            self.engine.set_project(self.replica.view());
        }
        changed
    }

    /// Edits of a track someone else has checked out would be refused by the server; they are
    /// refused here already, with a word why.
    fn allowed(&mut self, ops: &[Op]) -> bool {
        for op in ops {
            let track = match op.scope() {
                Scope::Project => continue,
                Scope::TrackContent(track) | Scope::TrackStructure(track) => track,
            };
            if let Some(why) = self.locked(track) {
                notice(&mut self.notices, &format!("This track is {why}."));
                return false;
            }
            if matches!(op.scope(), Scope::TrackContent(_)) && !self.replica.can_edit_content(track) {
                return false;
            }
        }
        true
    }

    /// Why this client cannot edit `track` right now, if it cannot.
    pub fn locked(&self, track: TrackId) -> Option<String> {
        let checkout = self.replica.confirmed().track(track)?.checkout?;
        if checkout.user != self.user {
            return Some(format!("checked out by {}", self.user_name(checkout.user)));
        }
        match self.replica.workspace(track) {
            Some(w) if w.committing => Some("being committed".into()),
            _ => None,
        }
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
        let before = self.replica.revision();
        let txn = self.replica.undo();
        self.after_edit(txn, before)
    }

    pub fn redo(&mut self) -> bool {
        let before = self.replica.revision();
        let txn = self.replica.redo();
        self.after_edit(txn, before)
    }

    /// Call once per UI frame.
    pub fn frame(&mut self) {
        self.receive();
        self.poll_jobs();
        self.keep_workspaces();
        self.share_presence();
        self.engine.set_project(self.replica.view());
        self.sync_transport();
        self.engine.tick();
    }

    /// Whether this client takes part in the shared transport. `None` without a server.
    pub fn linked(&self) -> Option<bool> {
        match &self.backend {
            Backend::Local(_) => None,
            Backend::Remote(remote) => Some(remote.linked),
        }
    }

    /// Joins or leaves the shared transport. Leaving, playback goes on privately from where it
    /// is; joining, it becomes what everyone else hears.
    pub fn set_linked(&mut self, linked: bool) {
        if let Backend::Remote(remote) = &mut self.backend {
            remote.linked = linked;
        }
        self.sync_transport();
    }

    /// Whether play, pause, seek and loop currently act on the shared transport: linked,
    /// connected, and the server's clock is known.
    pub fn is_sharing_transport(&self) -> bool {
        matches!(&self.backend, Backend::Remote(remote) if remote.sharing().is_some())
    }

    /// The shared transport as this client knows it, whether it takes part or not. `None`
    /// without a server.
    pub fn shared_transport(&self) -> Option<TransportState> {
        match &self.backend {
            Backend::Local(_) => None,
            Backend::Remote(remote) => Some(remote.transport),
        }
    }

    /// How well this client keeps time, while it shares the transport.
    pub fn timing(&self) -> Option<Timing> {
        let Backend::Remote(remote) = &self.backend else {
            return None;
        };
        let offset = remote.sharing()?;
        Some(Timing {
            server_micros: clock::now_micros() + offset,
            rtt_micros: remote.clock?.rtt_micros,
            drift_micros: self.engine.drift_micros(),
            resyncs: self.engine.stats().resyncs,
        })
    }

    /// Whether the transport is playing, as far as the user is concerned: a start that is
    /// scheduled counts.
    pub fn is_playing(&self) -> bool {
        match &self.backend {
            Backend::Remote(remote) if remote.sharing().is_some() => remote.transport.playing,
            _ => self.engine.is_playing(),
        }
    }

    /// The playhead in ticks.
    pub fn position(&self) -> f64 {
        self.engine.position()
    }

    /// The loop region of the transport this client follows, if looping is on.
    pub fn loop_range(&self) -> Option<TickRange> {
        match &self.backend {
            Backend::Remote(remote) if remote.sharing().is_some() => remote.transport.loop_range,
            _ => self.private_loop,
        }
    }

    pub fn play(&mut self) {
        let Some((remote, now)) = shared(&mut self.backend) else {
            self.engine.play();
            return;
        };
        if !remote.transport.playing {
            // Far enough ahead that everyone has heard of it when it is due.
            remote.request(TransportState {
                playing: true,
                anchor_server_micros: now + START_LEAD_MICROS,
                ..remote.transport
            });
            self.sync_transport();
        }
    }

    /// Stops where the playhead is.
    pub fn pause(&mut self) {
        let Some((remote, now)) = shared(&mut self.backend) else {
            self.engine.stop();
            return;
        };
        if remote.transport.playing {
            remote.request(TransportState {
                playing: false,
                anchor_tick: remote.transport.position(now, &self.tempo).round(),
                anchor_server_micros: now,
                ..remote.transport
            });
            self.sync_transport();
        }
    }

    pub fn seek(&mut self, tick: Tick) {
        let Some((remote, now)) = shared(&mut self.backend) else {
            self.engine.seek(tick);
            return;
        };
        remote.request(TransportState {
            anchor_tick: tick.0.max(0) as f64,
            anchor_server_micros: now + START_LEAD_MICROS,
            ..remote.transport
        });
        self.sync_transport();
    }

    /// Sets the loop region, or turns looping off.
    pub fn set_loop(&mut self, range: Option<TickRange>) {
        let range = range.filter(|range| !range.is_empty());
        let Some((remote, now)) = shared(&mut self.backend) else {
            self.private_loop = range;
            self.engine.set_loop(range);
            return;
        };
        if remote.transport.loop_range != range {
            // What the old loop did to the playhead so far stays done.
            remote.request(TransportState {
                loop_range: range,
                ..remote.transport.reanchored(now, &self.tempo)
            });
            self.sync_transport();
        }
    }

    /// Keeps the engine on the transport this client follows.
    fn sync_transport(&mut self) {
        let tempo = self.replica.view().tempo_map();
        let before = std::mem::replace(&mut self.tempo, tempo);
        let Backend::Remote(remote) = &mut self.backend else {
            return;
        };
        let heard = std::mem::take(&mut remote.transport_heard);
        let Some(offset) = remote.sharing() else {
            if remote.applied.take().is_some() {
                // On its own from here: the private transport starts as a copy.
                self.private_loop = remote.transport.loop_range;
                self.engine.unfollow();
            }
            return;
        };
        let now = clock::now_micros() + offset;
        if before != self.tempo && !heard {
            // The tempo changed under a running transport. The server will say where that
            // leaves the playhead; until then, and for an edit of our own that the server has
            // yet to see, the answer is: where it is now.
            remote.transport = remote.transport.reanchored(now, &before);
        }
        let state = remote.transport;
        let follow = |engine: &mut Engine, fresh| {
            engine.follow(state.anchor_tick, state.anchor_server_micros - offset, fresh)
        };
        match remote.applied {
            Some((applied, at)) if same_playback(&applied, &state) => {
                if state.playing && at != offset {
                    // The same playback, with a better idea of the server's clock.
                    follow(&mut self.engine, false);
                }
            }
            _ => {
                self.engine.set_loop(state.loop_range);
                if state.playing {
                    follow(&mut self.engine, true);
                } else {
                    if self.engine.is_playing() {
                        self.engine.stop();
                    }
                    self.engine.seek(Tick(state.anchor_tick.round() as i64));
                }
            }
        }
        remote.applied = Some((state, offset));
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
        // Progress to show: a sample playing or downloading, a commit on its way.
        let busy = self.playing.is_some() || self.wanted.is_some() || self.commit.is_some();
        if busy {
            return Some(Duration::from_millis(100));
        }
        // Leases are renewed even while nothing moves.
        let lease = (!self.saved.is_empty()).then_some(Duration::from_secs(LEASE_RENEW_SECS));
        (locks || self.editing.is_some() || remote.presence_sent.editing.is_some())
            .then_some(Duration::from_millis(250))
            .or(lease)
    }

    /// Where this project's checkouts and forks are kept. Journals found there are picked up
    /// by the checkouts they belong to.
    pub fn set_files(&mut self, files: Files) {
        self.forks = files.forks();
        self.files = Some(files);
        self.saved.clear();
        self.keep_workspaces();
    }

    /// Checkouts and commits are a matter between clients and a server.
    pub fn can_check_out(&self, track: TrackId) -> bool {
        self.online()
            && self
                .replica
                .confirmed()
                .track(track)
                .is_some_and(|t| t.checkout.is_none())
    }

    /// Who has `track` checked out, if anyone.
    pub fn checkout_owner(&self, track: TrackId) -> Option<UserId> {
        Some(self.replica.confirmed().track(track)?.checkout?.user)
    }

    /// True when the owner of the checkout of `track` has not been heard from for a while.
    pub fn is_stale(&self, track: TrackId) -> bool {
        matches!(&self.backend, Backend::Remote(remote) if remote.stale.contains(&track))
    }

    /// This client's checkout of `track`.
    pub fn workspace(&self, track: TrackId) -> Option<&Workspace> {
        self.replica.workspace(track)
    }

    pub fn check_out(&mut self, track: TrackId) {
        self.request(ClientMsg::Checkout { track });
    }

    /// Gives up this client's checkout of `track`, and the changes made in it.
    pub fn release(&mut self, track: TrackId) {
        if self.commit.as_ref().is_some_and(|c| c.track == track) {
            return;
        }
        self.request(ClientMsg::Release { track });
    }

    /// Ends someone else's checkout of `track`. Their changes stay with them.
    pub fn force_release(&mut self, track: TrackId) {
        self.request(ClientMsg::ForceRelease { track });
    }

    fn online(&self) -> bool {
        matches!(&self.backend, Backend::Remote(remote) if remote.status == Status::Online)
    }

    fn request(&mut self, msg: ClientMsg) {
        match &self.backend {
            Backend::Remote(remote) if remote.status == Status::Online => remote.link.send(remote.epoch, msg),
            _ => notice(&mut self.notices, "Not connected to the server."),
        }
    }

    /// Starts committing this client's checkout of `track`: no more edits go in, and the
    /// samples are rendered. The decision follows with [`Session::finish_commit`].
    pub fn start_commit(&mut self, track: TrackId) -> bool {
        if self.commit.is_some() || !self.online() {
            return false;
        }
        let Some(workspace) = self.replica.begin_commit(track) else {
            return false;
        };
        let confirmed = self.replica.confirmed().clone();
        self.commit = Some(Commit {
            track,
            plan: None,
            sent: None,
        });
        self.spawn(move || Job::Plan(checkout::plan_commit(&confirmed, &workspace)));
        true
    }

    pub fn commit_state(&self) -> Option<CommitState<'_>> {
        let commit = self.commit.as_ref()?;
        Some(match (&commit.plan, &commit.sent) {
            (_, Some(_)) => CommitState::Sending { track: commit.track },
            (Some(plan), None) => CommitState::Ready { plan },
            (None, None) => CommitState::Rendering { track: commit.track },
        })
    }

    /// Sends the commit once its samples are uploaded. With `restore_tempo` the tempo goes
    /// back to what it was when the checkout began, in the same step.
    pub fn finish_commit(&mut self, summary: &str, restore_tempo: bool) {
        let Some(commit) = self.commit.as_mut().filter(|c| c.sent.is_none()) else {
            return;
        };
        let Some(plan) = commit.plan.take() else {
            return;
        };
        let restore = restore_tempo && plan.conflict;
        let rendered = if restore { &plan.restored } else { &plan.as_is };
        let request = CommitRequest {
            track: plan.track,
            ops: plan.ops.clone(),
            project_fix: if restore {
                plan.restore_tempo.clone()
            } else {
                Vec::new()
            },
            stem: None,
            entry: ChangelogEntry {
                summary: summary.trim().to_string(),
                ranges: plan.ranges.clone(),
                samples: rendered.iter().map(|r| r.sample).collect(),
            },
        };
        let msg = ClientMsg::Commit(Box::new(request));
        commit.sent = Some(msg.clone());
        let blobs: Vec<BlobHash> = rendered.iter().map(|r| r.sample.blob).collect();
        for r in rendered.clone() {
            self.upload(&r);
        }
        self.send_after_uploads(blobs, msg);
    }

    /// Goes back to editing without committing. Too late once the commit is on its way.
    pub fn cancel_commit(&mut self) {
        if let Some(commit) = self.commit.take_if(|c| c.sent.is_none()) {
            self.replica.cancel_commit(commit.track);
        }
    }

    /// Checkouts, commits and releases, oldest first.
    pub fn log(&self) -> &[LogEntry] {
        &self.log
    }

    /// The samples of the changelog entry of event `seq`.
    pub fn samples(&self, seq: u64) -> &[Sample] {
        self.samples.get(&seq).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Plays a sample, fetching it first if need be. Playing it again stops it.
    pub fn play_sample(&mut self, sample: &Sample) {
        let blob = sample.blob;
        if self.playing.is_some_and(|(b, _)| b == blob) || self.wanted == Some(blob) {
            self.stop_sample();
            return;
        }
        self.engine.stop_preview();
        self.playing = None;
        match self.audio.get(&blob) {
            Some(audio) => {
                let id = self.engine.preview(audio.clone());
                self.playing = Some((blob, id));
            }
            None => {
                let Backend::Remote(remote) = &self.backend else {
                    return;
                };
                self.wanted = Some(blob);
                remote.link.get(blob);
            }
        }
    }

    /// Plays a sample rendered here, before it is uploaded.
    pub fn play_rendered(&mut self, rendered: &Rendered) {
        self.remember(rendered);
        self.play_sample(&rendered.sample);
    }

    pub fn stop_sample(&mut self) {
        self.wanted = None;
        if self.playing.take().is_some() {
            self.engine.stop_preview();
        }
    }

    /// How far the sample with `blob` has played, in seconds; `Some(0.0)` while it downloads.
    pub fn sample_position(&self, blob: BlobHash) -> Option<f64> {
        if self.wanted == Some(blob) {
            return Some(0.0);
        }
        let (playing, id) = self.playing?;
        (playing == blob).then(|| self.engine.preview_position(id))?
    }

    /// The work of checkouts someone else ended, kept on this machine.
    pub fn forks(&self) -> &[Fork] {
        &self.forks
    }

    /// Adds a fork to the project as a new track, and forgets it.
    pub fn restore_fork(&mut self, index: usize) -> bool {
        let Some(fork) = self.forks.get(index).cloned() else {
            return false;
        };
        let id = self.replica.ids().track();
        // Right below the track it came from, while that is still there.
        let view = self.replica.view();
        let after = match view.track(fork.track) {
            Some(_) => Some(fork.track),
            None => view.track_order.last().copied(),
        };
        let shell = TrackShell {
            name: format!("{} (fork)", fork.shell.name),
            color: fork.shell.color,
        };
        let op = Op::TrackAdd {
            id,
            shell,
            content: fork.content.clone(),
            after,
        };
        if !self.edit(vec![op]) {
            return false;
        }
        self.discard_fork(index);
        true
    }

    pub fn discard_fork(&mut self, index: usize) {
        if index < self.forks.len() {
            let fork = self.forks.remove(index);
            let _ = std::fs::remove_file(&fork.path);
        }
    }

    fn spawn(&self, work: impl FnOnce() -> Job + Send + 'static) {
        let (jobs, waker) = (self.jobs.0.clone(), self.waker.clone());
        std::thread::Builder::new()
            .name("mdaw-render".into())
            .spawn(move || {
                let _ = jobs.send(work());
                if let Some(waker) = waker {
                    waker();
                }
            })
            .expect("starting a render thread");
    }

    fn poll_jobs(&mut self) {
        while let Ok(job) = self.jobs.1.try_recv() {
            match job {
                Job::Plan(plan) => {
                    for r in plan.as_is.iter().chain(&plan.restored) {
                        let r = r.clone();
                        self.remember(&r);
                    }
                    if let Some(commit) = self.commit.as_mut().filter(|c| c.track == plan.track) {
                        commit.plan = Some(plan);
                    }
                }
                Job::Before { entry_seq, rendered } => {
                    self.upload(&rendered);
                    let attach = ClientMsg::Attach {
                        entry_seq,
                        sample: rendered.sample,
                    };
                    self.send_after_uploads(vec![rendered.sample.blob], attach);
                }
                Job::Nothing => {}
            }
        }
    }

    /// Keeps a sample rendered here for playing.
    fn remember(&mut self, rendered: &Rendered) {
        let blob = rendered.sample.blob;
        if !self.audio.contains_key(&blob) {
            let audio = flac::Audio {
                samples: rendered.audio.to_vec(),
                sample_rate: checkout::SAMPLE_RATE,
            };
            let resampled = flac::resample(&audio, self.engine.sample_rate());
            self.audio.insert(blob, resampled.into());
        }
    }

    fn upload(&mut self, rendered: &Rendered) {
        self.remember(rendered);
        let blob = rendered.sample.blob;
        if self.uploads.contains_key(&blob) {
            return;
        }
        self.uploads.insert(blob, rendered.flac.clone());
        if let Backend::Remote(remote) = &self.backend {
            remote.link.put(blob, rendered.flac.clone());
        }
    }

    fn send_after_uploads(&mut self, blobs: Vec<BlobHash>, msg: ClientMsg) {
        self.outgoing.push((blobs, msg));
        self.flush_outgoing();
    }

    /// Sends the messages whose blobs have all arrived at the server.
    fn flush_outgoing(&mut self) {
        let Backend::Remote(remote) = &self.backend else {
            return;
        };
        if remote.status != Status::Online {
            return;
        }
        let uploads = &self.uploads;
        self.outgoing.retain(|(blobs, msg)| {
            let ready = blobs.iter().all(|b| !uploads.contains_key(b));
            if ready {
                remote.link.send(remote.epoch, msg.clone());
            }
            !ready
        });
    }

    /// Writes journals that changed, renews leases, and deals with checkouts that ended.
    fn keep_workspaces(&mut self) {
        let tracks: Vec<TrackId> = self.replica.workspaces().map(|w| w.track).collect();
        for track in tracks {
            if !self.saved.contains_key(&track) {
                // New to this session: the work may have been saved by an earlier run.
                let earlier = self.files.as_ref().and_then(|f| f.load_journal(track));
                if let Some((journal, base_tempo)) = earlier {
                    let empty = self
                        .replica
                        .workspace(track)
                        .is_some_and(|w| w.journal.is_empty());
                    if empty && !journal.is_empty() {
                        self.replica.restore_journal(track, journal, base_tempo);
                        notice(
                            &mut self.notices,
                            "Picked up the changes of your checkout from before.",
                        );
                    }
                }
            }
            let workspace = self.replica.workspace(track).expect("listed above");
            let len = workspace.journal.len();
            if self.saved.get(&track) != Some(&len) {
                if let Some(files) = &self.files
                    && let Err(err) = files.save_journal(workspace)
                {
                    notice(
                        &mut self.notices,
                        &format!("Could not save the checkout: {err:#}."),
                    );
                }
                self.saved.insert(track, len);
            }
        }

        for (workspace, how) in self.replica.take_ended() {
            let track = workspace.track;
            self.saved.remove(&track);
            if self.commit.as_ref().is_some_and(|c| c.track == track) {
                self.commit = None;
            }
            let keep = match how {
                Ended::Committed | Ended::Abandoned => false,
                Ended::Forced { by } => {
                    let text = format!(
                        "{} ended your checkout. Your changes are kept as a fork in the changelog.",
                        self.user_name(by)
                    );
                    notice(&mut self.notices, &text);
                    true
                }
                Ended::Lost => true,
            };
            if keep && !workspace.journal.is_empty() {
                self.save_fork(&workspace);
            }
            if let Some(files) = &self.files {
                files.remove_journal(track);
            }
        }

        if self.online() && self.lease_sent.elapsed() >= Duration::from_secs(LEASE_RENEW_SECS) {
            let tracks: Vec<TrackId> = self.replica.workspaces().map(|w| w.track).collect();
            if let Backend::Remote(remote) = &self.backend {
                for track in tracks {
                    remote.link.send(remote.epoch, ClientMsg::LeaseRenew { track });
                }
            }
            self.lease_sent = Instant::now();
        }
    }

    /// Keeps the work of a checkout that ended without a commit.
    fn save_fork(&mut self, workspace: &Workspace) {
        let confirmed = self.replica.confirmed();
        // The contents are as they were at the checkout: nobody could change them since.
        let Some(track) = confirmed.track(workspace.track) else {
            return;
        };
        let mut project = Project::default();
        let id = workspace.track;
        apply(
            &mut project,
            &Op::TrackAdd {
                id,
                shell: track.shell.clone(),
                content: (*track.content).clone(),
                after: None,
            },
        );
        apply_all(&mut project, &workspace.journal);
        let Some(forked) = project.track(id) else { return };
        let mut fork = Fork {
            track: id,
            shell: forked.shell.clone(),
            content: (*forked.content).clone(),
            saved: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            path: Default::default(),
        };
        if let Some(files) = &self.files {
            match files.save_fork(&fork) {
                Ok(path) => fork.path = path,
                Err(err) => notice(&mut self.notices, &format!("Could not save the fork: {err:#}.")),
            }
        }
        self.forks.push(fork);
    }

    /// A commit message that may have gone down with a connection goes again, unless the
    /// commit made it into the log meanwhile.
    fn resend_commit(&mut self) {
        let Some(commit) = &self.commit else { return };
        let Some(msg) = commit.sent.clone() else { return };
        if !self.replica.workspace(commit.track).is_some_and(|w| w.committing) {
            return;
        }
        let pending = self.outgoing.iter().any(|(_, m)| *m == msg);
        if !pending {
            self.outgoing.push((Vec::new(), msg));
        }
    }

    /// Takes in the changelog a fresh copy of the project comes with.
    fn absorb_history(&mut self, history: Vec<Event>) {
        self.log.clear();
        self.samples.clear();
        self.displaced.clear();
        for event in history {
            self.note_event(&event, false);
            record_displaced(&mut self.displaced, event);
        }
    }

    /// Notes what the changelog shows of an event. With `fresh`, the event has just happened,
    /// and edits of this user it displaced get a sample of how they sounded.
    fn note_event(&mut self, event: &Event, fresh: bool) {
        let (seq, by, wall_ts) = (event.seq, event.user, event.wall_ts);
        let what = match &event.body {
            EventBody::Txn { superseded, .. } => {
                if fresh && matches!(self.backend, Backend::Remote(_)) {
                    self.render_before(seq, superseded);
                }
                None
            }
            EventBody::Checkout { track } => Some(Logged::Checkout { track: *track }),
            EventBody::Commit { track, entry, .. } => {
                self.samples
                    .entry(seq)
                    .or_default()
                    .extend(entry.samples.iter().copied());
                Some(Logged::Commit {
                    track: *track,
                    summary: entry.summary.clone(),
                    ranges: entry.ranges.clone(),
                })
            }
            EventBody::Release { track, reason } => Some(Logged::Release {
                track: *track,
                reason: *reason,
            }),
            EventBody::Attach { entry_seq, sample } => {
                self.samples.entry(*entry_seq).or_default().push(*sample);
                None
            }
        };
        if let Some(what) = what {
            self.log.push(LogEntry {
                seq,
                by,
                wall_ts,
                what,
            });
            if self.log.len() > MAX_LOG {
                let old = self.log.remove(0);
                self.samples.remove(&old.seq);
            }
        }
    }

    /// Renders how this user's displaced edits sounded, one sample per track.
    fn render_before(&mut self, seq: u64, superseded: &[Superseded]) {
        let mut tracks = BTreeSet::new();
        for lost in superseded.iter().filter(|l| l.lost_user == self.user) {
            let Some(track) = lost.entity.track() else {
                continue;
            };
            if !tracks.insert(track) {
                continue;
            }
            let (doc, lost) = (self.replica.confirmed().clone(), lost.clone());
            self.spawn(move || match checkout::render_before(&doc, &lost) {
                Some(rendered) => Job::Before {
                    entry_seq: seq,
                    rendered,
                },
                None => Job::Nothing,
            });
        }
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
        self.sync_transport();
        true
    }

    /// Takes in what arrived from the server since the last frame.
    fn receive(&mut self) {
        loop {
            let Backend::Remote(remote) = &mut self.backend else {
                return;
            };
            let Some(event) = remote.link.poll() else {
                return;
            };
            match event {
                LinkEvent::Welcome { epoch, welcome } => {
                    let Welcome {
                        actor,
                        user,
                        head_seq,
                        sync,
                        acked,
                        peers,
                        transport,
                        history,
                        stale,
                        ..
                    } = *welcome;
                    remote.stale = stale;
                    let caught_up = match &sync {
                        SyncData::Events(events) => Some(events.clone()),
                        SyncData::Snapshot(_) => None,
                    };
                    remote.epoch = epoch;
                    // A new connection measures the clock anew; the transport is shared again
                    // once it has.
                    remote.clock = None;
                    // Requests that went down with the old connection get no answer.
                    remote.transport_asked = 0;
                    remote.transport = transport;
                    remote.transport_heard = true;
                    remote.set_peers(peers);
                    remote.presence_sent = Presence::default();
                    self.user = user;
                    match self.replica.resync(actor, head_seq, sync, acked) {
                        Ok(resend) => {
                            remote.status = Status::Online;
                            let reloaded = std::mem::take(&mut remote.desynced);
                            for txn in resend {
                                remote.link.send(epoch, ClientMsg::Submit(txn));
                            }
                            // Uploads that went down with the old connection start over.
                            for (hash, bytes) in &self.uploads {
                                remote.link.put(*hash, bytes.clone());
                            }
                            if reloaded {
                                // The undo history refers to a document that is gone.
                                notice(&mut self.notices, "Reloaded the project from the server.");
                            }
                            match caught_up {
                                Some(events) => {
                                    for event in events {
                                        self.note_event(&event, false);
                                        record_displaced(&mut self.displaced, event);
                                    }
                                }
                                None => self.absorb_history(history),
                            }
                            self.resend_commit();
                            self.flush_outgoing();
                        }
                        Err(_) => remote.lose_track(),
                    }
                }
                LinkEvent::Clock(estimate) => remote.clock = Some(estimate),
                // The transport is not part of the document: it holds even while the replica
                // waits for a fresh copy.
                LinkEvent::Msg(ServerMsg::Transport { state, by }) => {
                    if by == Some(self.replica.actor()) {
                        remote.transport_asked = remote.transport_asked.saturating_sub(1);
                    }
                    // The server takes requests in order: as long as one of ours is on its
                    // way, it will replace this state, and showing it would only be a flicker.
                    if remote.transport_asked == 0 {
                        remote.transport = state;
                        remote.transport_heard = true;
                    }
                }
                LinkEvent::Msg(_) if remote.desynced => {}
                LinkEvent::Msg(ServerMsg::Event(event)) => match self.replica.on_event(&event) {
                    Ok(()) => {
                        self.note_event(&event, true);
                        record_displaced(&mut self.displaced, event);
                    }
                    Err(_) => remote.lose_track(),
                },
                LinkEvent::Msg(ServerMsg::Denied { request, reason }) => {
                    if let Request::Commit(track) = request
                        && self.commit.as_ref().is_some_and(|c| c.track == track)
                    {
                        self.commit = None;
                        self.replica.cancel_commit(track);
                    }
                    let what = match request {
                        Request::Checkout(_) => "The checkout",
                        Request::Release(_) | Request::ForceRelease(_) => "The release",
                        Request::Commit(_) => "The commit",
                        Request::Attach(_) => "A sample",
                    };
                    notice(&mut self.notices, &format!("{what} was not accepted: {reason}."));
                }
                LinkEvent::Msg(ServerMsg::Stale(stale)) => remote.stale = stale,
                LinkEvent::Stored { hash, ok } => {
                    if ok {
                        self.uploads.remove(&hash);
                        self.flush_outgoing();
                    }
                    // Otherwise it goes again with the next connection.
                }
                LinkEvent::Fetched { hash, bytes } => {
                    let audio = bytes.and_then(|b| flac::decode(&b).ok());
                    let Some(audio) = audio else {
                        if self.wanted.take() == Some(hash) {
                            notice(&mut self.notices, "Could not get the sample from the server.");
                        }
                        continue;
                    };
                    let resampled: Arc<[f32]> = flac::resample(&audio, self.engine.sample_rate()).into();
                    self.audio.insert(hash, resampled.clone());
                    if self.wanted == Some(hash) {
                        self.wanted = None;
                        let id = self.engine.preview(resampled);
                        self.playing = Some((hash, id));
                    }
                }
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
                    remote.clock = None;
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

    /// The offset of the server's clock from the engine's, if this client shares the
    /// transport right now.
    fn sharing(&self) -> Option<i64> {
        let clock = self
            .clock
            .filter(|_| self.linked && self.status == Status::Online)?;
        Some(clock.offset_micros)
    }

    /// Asks the server for a new state of the shared transport, and acts as if it had agreed:
    /// whatever it answers replaces this.
    fn request(&mut self, state: TransportState) {
        self.transport = state;
        self.transport_asked += 1;
        self.link.send(self.epoch, ClientMsg::SetTransport(state));
    }

    /// The replica and the server's log no longer fit together: start over with a snapshot.
    fn lose_track(&mut self) {
        self.desynced = true;
        self.status = Status::Reconnecting;
        self.link.rejoin();
    }
}

/// The shared transport and the server's time now, if this client shares it.
fn shared(backend: &mut Backend) -> Option<(&mut Remote, i64)> {
    let Backend::Remote(remote) = backend else {
        return None;
    };
    let offset = remote.sharing()?;
    Some((remote, clock::now_micros() + offset))
}

/// Whether two states put the playhead in the same place at all times. The epoch only counts
/// the server's answers, and a playhead at rest is nowhere else at another time.
fn same_playback(a: &TransportState, b: &TransportState) -> bool {
    let place = |s: &TransportState| (s.playing, s.anchor_tick, s.loop_range);
    place(a) == place(b) && (!a.playing || a.anchor_server_micros == b.anchor_server_micros)
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
    let (seq, by, wall_ts) = (event.seq, event.user, event.wall_ts);
    let EventBody::Txn { superseded, .. } = event.body else {
        return;
    };
    displaced.extend(superseded.into_iter().map(|lost| Displaced {
        seq,
        by,
        wall_ts,
        lost,
    }));
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
