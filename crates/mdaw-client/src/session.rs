//! One editing session: the replica, the thing that orders its edits, and the audio engine.
//!
//! Playback is this client's own: play, stop, seek and loop act on the local engine and nothing
//! about them goes over the network.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use mdaw_dsp::pocket;
use mdaw_engine::{Engine, clock};
use mdaw_model::{
    ActorId, BlobHash, BuiltinKind, Device, DeviceId, DeviceKind, EntityRef, IdGen, Op, Project, TempoMap,
    TempoPoint, Tick, TickRange, TrackContent, TrackId, TrackShell, UserId, apply, apply_all,
};
use mdaw_net::JoinInfo;
use mdaw_proto::{ClientMsg, Event, Peer, Presence, ServerMsg, Superseded, SyncData, Txn, Welcome};
use mdaw_sync::{Gesture, Loopback, Replica};

use crate::link::{Link, LinkEvent, Waker, Who};
use crate::live::Live;
use crate::plugins::{self, Plugins, TrackSound};
use crate::record::{Take, take_ops};
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
/// How often the main loop runs at least while a plugin's own window is open: plugins draw from
/// their timers, which run only then.
const PLUGIN_WINDOW_PACE: Duration = Duration::from_millis(10);
/// How often it runs at least while plugins run without a window, for the callbacks they ask for.
const PLUGIN_PACE: Duration = Duration::from_millis(50);

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
    /// The tempo map of the view, as of the last frame.
    tempo: TempoMap,
    /// The loop of this client's transport.
    loop_range: Option<TickRange>,
    waker: Option<Waker>,
    /// Blobs made here and not yet known to be on the server, with their bytes.
    uploads: HashMap<BlobHash, Arc<[u8]>>,
    /// Transactions to send, in order, once none of the blobs they name is still uploading:
    /// the server refuses an edit that names a blob it does not have.
    outgoing: Vec<Txn>,
    /// Blobs being downloaded.
    fetching: HashSet<BlobHash>,
    live: Live,
    /// Recording is armed: notes this player plays while the transport runs are kept.
    recording: bool,
    /// The notes of the current run of the transport, while recording.
    take: Take,
    plugins: plugins::Shared,
    /// When the plugins' settings were last looked at for changes.
    plugins_saved_at: Instant,
    /// Saves of one plugin's settings, one after the other, are one undo step.
    plugin_gestures: HashMap<(TrackId, DeviceId), Gesture>,
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
                    | LinkEvent::Stored { .. }
                    | LinkEvent::Fetched { .. }
                    | LinkEvent::TooLarge(_),
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
            history,
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

    fn new(replica: Replica, backend: Backend, user: UserId, mut engine: Engine) -> Self {
        let live = Live::new(engine.live_input());
        let plugins = plugins::Shared(Rc::new(RefCell::new(Plugins::new(engine.sample_rate()))));
        engine.set_plugins(Box::new(plugins.clone()));
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
            loop_range: None,
            waker: None,
            uploads: HashMap::new(),
            outgoing: Vec::new(),
            fetching: HashSet::new(),
            live,
            recording: false,
            take: Take::default(),
            plugins,
            plugins_saved_at: Instant::now(),
            plugin_gestures: HashMap::new(),
        };
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
        let txn = self.replica.submit(ops);
        self.send(txn)
    }

    /// Like [`Self::edit`], but edits with the same gesture collapse into one undo step. Use it
    /// for continuous controls, so the change is audible while the control moves.
    pub fn edit_gesture(&mut self, ops: Vec<Op>, gesture: Gesture) -> bool {
        if !self.allowed(&ops) {
            return false;
        }
        let txn = self.replica.submit_gesture(ops, gesture);
        self.send(txn)
    }

    /// Edits the server would refuse as out of range or past the limits of the project are
    /// refused here already, with a word why.
    fn allowed(&mut self, ops: &[Op]) -> bool {
        let mut doc = self.replica.view().clone();
        for op in ops {
            if let Err(why) = op.validate().and_then(|()| op.fits(&doc)) {
                notice(&mut self.notices, &format!("This edit cannot be made: {why}."));
                return false;
            }
            apply(&mut doc, op);
        }
        true
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
        self.tempo = self.replica.view().tempo_map();
        self.engine.tick();
        self.keep_plugins();
        self.record();
    }

    /// Hosts plugins from now on: tracks whose plugins are installed here are heard. Without
    /// this, every track with plugins is silent.
    pub fn enable_plugins(&mut self, host: mdaw_plugin::Host) {
        let mut plugins = self.plugins.0.borrow_mut();
        plugins.set_host(Some(host));
        if let Some(waker) = &self.waker {
            plugins.set_waker(waker.clone());
        }
        drop(plugins);
        self.engine.refresh();
    }

    pub fn plugins_enabled(&self) -> bool {
        self.plugins.0.borrow().has_host()
    }

    /// The plugins installed here.
    pub fn plugin_catalog(&self) -> Vec<mdaw_plugin::PluginInfo> {
        self.plugins.0.borrow().catalog().to_vec()
    }

    /// The name of a plugin if it is installed here.
    pub fn plugin_name(&self, format: mdaw_model::PluginFormat, id: &str) -> Option<String> {
        Some(self.plugins.0.borrow().installed(format, id)?.name.clone())
    }

    /// How a track is heard here: from its devices or its plugins, or not at all.
    pub fn track_sound(&self, track: TrackId) -> TrackSound {
        self.plugins.0.borrow().sound(self.replica.view(), track)
    }

    /// Whether a device makes its track's sound, as opposed to changing it. A plugin that is not
    /// installed here counts as one.
    fn is_instrument(&self, device: &mdaw_model::Device) -> bool {
        match &device.kind {
            DeviceKind::Builtin { .. } => true,
            DeviceKind::Plugin { format, id, .. } => self
                .plugins
                .0
                .borrow()
                .installed(*format, id)
                .is_none_or(|p| p.instrument),
        }
    }

    /// The ops that make `device` the track's only instrument: the others go, it comes first.
    fn replace_instrument(&mut self, track: TrackId, device: mdaw_model::Device) -> Vec<Op> {
        let Some(content) = self.replica.view().track(track).map(|t| t.content.clone()) else {
            return Vec::new();
        };
        let mut ops: Vec<Op> = content
            .devices
            .iter()
            .filter(|d| self.is_instrument(d))
            .map(|d| Op::DeviceRemove { track, id: d.id })
            .collect();
        ops.push(Op::DeviceAdd {
            track,
            device,
            after: None,
        });
        ops
    }

    /// Puts a plugin on a track: an instrument in place of the track's instrument, an effect at
    /// the end.
    pub fn add_plugin(&mut self, track: TrackId, plugin: &mdaw_plugin::PluginInfo) -> bool {
        let device = mdaw_model::Device::new(
            self.replica.ids().device(),
            DeviceKind::Plugin {
                format: plugin.format,
                id: plugin.id.clone(),
                state: None,
            },
        );
        if plugin.instrument {
            let ops = self.replace_instrument(track, device);
            return self.edit(ops);
        }
        let after = self
            .replica
            .view()
            .track(track)
            .and_then(|t| t.content.devices.last().map(|d| d.id));
        self.edit(vec![Op::DeviceAdd { track, device, after }])
    }

    /// Makes the built-in Pocket synth the track's instrument again.
    pub fn use_pocket(&mut self, track: TrackId) -> bool {
        let kind = DeviceKind::Builtin {
            kind: BuiltinKind::Pocket,
            dsp_version: pocket::DSP_VERSION,
        };
        let device = mdaw_model::Device::new(self.replica.ids().device(), kind);
        let ops = self.replace_instrument(track, device);
        self.edit(ops)
    }

    /// The first `limit` parameters of a plugin device running here, and how many it has.
    pub fn plugin_params(
        &mut self,
        track: TrackId,
        device: mdaw_model::DeviceId,
        limit: usize,
    ) -> Option<(Vec<mdaw_plugin::ParamInfo>, usize)> {
        self.plugins.0.borrow_mut().params(track, device, limit)
    }

    /// Gives running plugins what they expect from the main loop: their timers, the callbacks
    /// they asked for, their windows' events. Call as often as possible, also between frames
    /// and while the window is hidden; [`Session::frame`] does it too.
    pub fn pump_plugins(&mut self) {
        let crashed = self.plugins.0.borrow_mut().idle();
        for ((track, device), what) in crashed {
            // Its processor is dead: the next graph does without it, and the track is silent.
            self.engine.forget(track, device);
            let text = format!(
                "{what} Its track is silent here until it is restarted; Restart is in the Device tab."
            );
            notice(&mut self.notices, &text);
        }
        if self.plugins.0.borrow_mut().take_dirty() {
            self.engine.refresh();
        }
    }

    /// Whether a plugin device crashed and waits to be restarted.
    pub fn plugin_crashed(&self, track: TrackId, device: mdaw_model::DeviceId) -> bool {
        self.plugins.0.borrow().crashed(track, device)
    }

    /// Starts a crashed plugin again, with its last settings.
    pub fn restart_plugin(&mut self, track: TrackId, device: mdaw_model::DeviceId) {
        self.plugins.0.borrow_mut().restart(track, device);
        self.engine.forget(track, device);
        self.engine.refresh();
    }

    /// Sets a parameter of a plugin running here. The plugin's state changes, and reaches the
    /// document with the next save of its settings.
    pub fn set_plugin_param(&mut self, track: TrackId, device: mdaw_model::DeviceId, param: u32, value: f64) {
        self.plugins.0.borrow_mut().set_param(track, device, param, value);
    }

    /// Opens or closes a plugin's own window.
    pub fn toggle_plugin_editor(&mut self, track: TrackId, device: mdaw_model::DeviceId) {
        let title = self
            .replica
            .view()
            .track(track)
            .map(|t| t.shell.name.clone())
            .unwrap_or_default();
        let mut plugins = self.plugins.0.borrow_mut();
        let Some(instance) = plugins.instance(track, device) else {
            return;
        };
        if instance.editor_open() {
            instance.close_editor();
        } else {
            let title = format!("{} - {title}", instance.info().name);
            if let Err(err) = instance.open_editor(&title) {
                drop(plugins);
                notice(
                    &mut self.notices,
                    &format!("The plugin's window did not open: {err}."),
                );
            }
        }
    }

    pub fn plugin_editor_open(&self, track: TrackId, device: mdaw_model::DeviceId) -> bool {
        self.plugins
            .0
            .borrow_mut()
            .instance(track, device)
            .is_some_and(|i| i.editor_open())
    }

    /// Whether a plugin running here has a window of its own.
    pub fn plugin_has_editor(&self, track: TrackId, device: mdaw_model::DeviceId) -> bool {
        self.plugins
            .0
            .borrow_mut()
            .instance(track, device)
            .is_some_and(|i| i.has_editor())
    }

    /// Whether a plugin device runs here.
    pub fn plugin_running(&self, track: TrackId, device: mdaw_model::DeviceId) -> bool {
        self.plugins.0.borrow_mut().instance(track, device).is_some()
    }

    /// Keeps the plugins in step with the document, fetches the state blobs they need, and puts
    /// the settings of plugins changed here into the document.
    fn keep_plugins(&mut self) {
        let renew = self.plugins.0.borrow_mut().follow(self.replica.view());
        for &(track, device) in &renew {
            self.engine.forget(track, device);
        }
        self.pump_plugins();
        if let Backend::Remote(remote) = &self.backend
            && remote.status == Status::Online
        {
            let wanted = self.plugins.0.borrow_mut().take_wanted(&self.fetching);
            for hash in wanted {
                self.fetching.insert(hash);
                remote.link.get(hash);
            }
        }
        if self.plugins.0.borrow_mut().take_dirty() || !renew.is_empty() {
            self.engine.refresh();
        }
        if self.plugins_saved_at.elapsed() >= plugins::SAVE_EVERY {
            self.plugins_saved_at = Instant::now();
            self.save_plugin_states();
        }
    }

    /// Saves the settings of plugins that changed here, uploads them and records them in the
    /// document. The edit goes to the server once the upload has arrived.
    pub fn save_plugin_states(&mut self) {
        let saved = self.plugins.0.borrow_mut().save_changed();
        for ((track, id), (hash, bytes)) in saved {
            self.put_blob(hash, bytes);
            let gesture = match self.plugin_gestures.get(&(track, id)) {
                Some(gesture) => *gesture,
                None => {
                    let gesture = self.new_gesture();
                    self.plugin_gestures.insert((track, id), gesture);
                    gesture
                }
            };
            let op = Op::DeviceSetState {
                track,
                id,
                state: Some(hash),
            };
            self.edit_gesture(vec![op], gesture);
        }
    }

    /// Uploads a blob made here, once, now or on the next connection.
    fn put_blob(&mut self, hash: BlobHash, bytes: Arc<[u8]>) {
        let Backend::Remote(remote) = &self.backend else {
            return;
        };
        if self.uploads.contains_key(&hash) {
            return;
        }
        self.uploads.insert(hash, bytes.clone());
        remote.link.put(hash, bytes);
    }

    /// Live notes: what this player plays, heard at once.
    pub fn live(&self) -> &Live {
        &self.live
    }

    /// Whether recording is armed.
    pub fn is_recording(&self) -> bool {
        self.recording
    }

    /// Arms or disarms recording. While armed, the notes this player plays while the transport
    /// runs become one edit when it stops, or when recording is disarmed.
    pub fn set_recording(&mut self, on: bool) {
        if self.recording == on {
            return;
        }
        if !on {
            self.record();
            self.finish_take();
        }
        self.recording = on;
        self.live.set_capturing(on);
    }

    /// Notes in the take so far.
    pub fn take_len(&self) -> usize {
        self.take.len()
    }

    fn record(&mut self) {
        if !self.recording {
            return;
        }
        for played in self.live.take_played() {
            if let Some(tick) = self.tick_at(played.micros) {
                self.take.add(&played, tick);
            }
        }
        match self.tick_at(clock::now_micros()) {
            Some(tick) => {
                let range = self.loop_range();
                self.take.playhead(tick, range);
            }
            None => self.finish_take(),
        }
    }

    fn finish_take(&mut self) {
        let take = std::mem::take(&mut self.take);
        if take.is_empty() {
            return;
        }
        let notes = take.finish();
        let project = self.replica.view().clone();
        let ops = take_ops(&project, self.replica.ids(), &notes);
        if !ops.is_empty() {
            self.edit(ops);
        }
    }

    /// Where the transport was, or is, at `micros` on [`clock::now_micros`]. `None` unless it
    /// runs then.
    fn tick_at(&self, micros: i64) -> Option<f64> {
        if !self.engine.is_playing() {
            return None;
        }
        // From where the playhead was at the time it was heard.
        let (tick, heard_at) = self.engine.position_heard();
        let since = (micros - heard_at) as f64 / 1e6;
        let then = self.tempo.tick_to_seconds(tick) + since;
        Some(self.tempo.seconds_to_tick(then.max(0.0)))
    }

    /// Whether this client's transport is playing.
    pub fn is_playing(&self) -> bool {
        self.engine.is_playing()
    }

    /// The playhead in ticks.
    pub fn position(&self) -> f64 {
        self.engine.position()
    }

    /// The loop region, if looping is on.
    pub fn loop_range(&self) -> Option<TickRange> {
        self.loop_range
    }

    pub fn play(&mut self) {
        self.engine.play();
    }

    /// Stops where the playhead is.
    pub fn pause(&mut self) {
        self.engine.stop();
    }

    pub fn seek(&mut self, tick: Tick) {
        self.engine.seek(tick);
    }

    /// Sets the loop region, or turns looping off.
    pub fn set_loop(&mut self, range: Option<TickRange>) {
        let range = range.filter(|range| !range.is_empty());
        self.loop_range = range;
        self.engine.set_loop(range);
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
        // Plugins live on this thread: their windows, timers and callbacks run only when it
        // does, also while nothing changes in this window.
        let plugins = match self.plugins.0.borrow().activity() {
            (_, true) => Some(PLUGIN_WINDOW_PACE),
            (true, false) => Some(PLUGIN_PACE),
            _ => None,
        };
        let others = self.others_next_frame_in();
        match (plugins, others) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn others_next_frame_in(&self) -> Option<Duration> {
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

    /// Sends what waits in `outgoing`, in order, as long as the blobs each names have arrived.
    fn flush_outgoing(&mut self) {
        let Backend::Remote(remote) = &self.backend else {
            return;
        };
        if remote.status != Status::Online {
            return;
        }
        let uploads = &self.uploads;
        let ready = self
            .outgoing
            .iter()
            .take_while(|txn| {
                let blobs = txn.ops.iter().flat_map(Op::blobs);
                !blobs.into_iter().any(|b| uploads.contains_key(&b))
            })
            .count();
        for txn in self.outgoing.drain(..ready) {
            remote.link.send(remote.epoch, ClientMsg::Submit(txn));
        }
    }

    /// Takes in the history a fresh copy of the project comes with.
    fn absorb_history(&mut self, history: Vec<Event>) {
        self.displaced.clear();
        for event in history {
            record_displaced(&mut self.displaced, event);
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
                    self.outgoing.push(txn);
                    self.flush_outgoing();
                }
            }
        }
        self.engine.set_project(self.replica.view());
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
                        history,
                        ..
                    } = *welcome;
                    let caught_up = match &sync {
                        SyncData::Events(events) => Some(events.clone()),
                        SyncData::Snapshot(_) => None,
                    };
                    remote.epoch = epoch;
                    remote.set_peers(peers);
                    remote.presence_sent = Presence::default();
                    self.user = user;
                    match self.replica.resync(actor, head_seq, sync, acked) {
                        Ok(resend) => {
                            remote.status = Status::Online;
                            let reloaded = std::mem::take(&mut remote.desynced);
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
                                        record_displaced(&mut self.displaced, event);
                                    }
                                }
                                None => self.absorb_history(history),
                            }
                            // Everything pending goes again, renumbered, behind its blobs.
                            self.outgoing = resend;
                            self.flush_outgoing();
                        }
                        Err(_) => remote.lose_track(),
                    }
                }
                LinkEvent::Msg(_) if remote.desynced => {}
                LinkEvent::Msg(ServerMsg::Event(event)) => match self.replica.on_event(&event) {
                    Ok(()) => record_displaced(&mut self.displaced, event),
                    Err(_) => remote.lose_track(),
                },
                LinkEvent::Stored { hash, ok } => {
                    if ok {
                        self.uploads.remove(&hash);
                        self.flush_outgoing();
                    }
                    // Otherwise it goes again with the next connection.
                }
                LinkEvent::Fetched { hash, bytes } => {
                    self.fetching.remove(&hash);
                    let mut plugins = self.plugins.0.borrow_mut();
                    match bytes {
                        Some(bytes) if plugins.wanted(&hash) => plugins.add_state(hash, bytes),
                        Some(_) => {}
                        None => plugins.give_up(&hash),
                    }
                }
                LinkEvent::Msg(ServerMsg::Rejected {
                    client_txn_id,
                    reason,
                }) => {
                    self.replica.on_rejected(client_txn_id);
                    notice(&mut self.notices, &format!("An edit was not accepted: {reason}."));
                }
                LinkEvent::TooLarge(ClientMsg::Submit(txn)) => {
                    self.replica.on_rejected(txn.client_txn_id);
                    notice(
                        &mut self.notices,
                        "An edit was too large to send and has been taken back.",
                    );
                }
                LinkEvent::TooLarge(_) => {
                    notice(&mut self.notices, "A message was too large to send.");
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
    let (seq, by, wall_ts) = (event.seq, event.user, event.wall_ts);
    displaced.extend(event.superseded.into_iter().map(|lost| Displaced {
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
