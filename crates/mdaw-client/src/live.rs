//! Live notes: what this player plays, heard here at once and sent to the others, and what the
//! others play, heard here as it arrives.
//!
//! [`Live`] is shared by the UI, the network thread and the thread of a MIDI device, so a note
//! never waits for a frame of the UI. The UI only decides which track notes go to, and which
//! tracks do not listen to the others.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use mdaw_engine::midi::{self, MidiDevice, MidiMsg};
use mdaw_engine::{LiveInput, clock};
use mdaw_model::{ActorId, TrackId};
use mdaw_proto::{Datagram, LiveAction, MidiIn, MidiOut, MidiPacket};

/// Engine keys of this player's notes: the pitch is added.
const OWN_KEY: u64 = 1 << 40;
/// Engine keys of others' notes: the actor, shifted by 8, and the pitch are added.
const REMOTE_KEY: u64 = 1 << 41;
/// How often the network thread lets [`Live::tick`] run.
pub const TICK: Duration = Duration::from_millis(50);

/// A note this player started or ended, with when, for recording.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Played {
    /// On [`clock::now_micros`].
    pub micros: i64,
    pub track: TrackId,
    pub pitch: u8,
    /// 0 for an end.
    pub vel: u8,
}

#[derive(Clone)]
pub struct Live {
    state: Arc<Mutex<State>>,
    engine: LiveInput,
    /// Tells the UI that others started or ended notes, so it can show who plays.
    waker: Option<crate::link::Waker>,
}

#[derive(Default)]
struct State {
    out: MidiOut,
    inn: MidiIn,
    connection: Option<quinn::Connection>,
    /// The track notes go to.
    target: Option<TrackId>,
    /// Tracks that do not hear the others.
    deaf: BTreeSet<TrackId>,
    /// Who is there; others' notes are silenced when they leave.
    present: BTreeSet<ActorId>,
    /// This player's notes that sound, by pitch.
    held: BTreeMap<u8, TrackId>,
    /// Kept for recording while `capturing`.
    played: Vec<Played>,
    capturing: bool,
}

impl Live {
    pub fn new(engine: LiveInput) -> Self {
        Self {
            state: Arc::default(),
            engine,
            waker: None,
        }
    }

    /// The UI is woken through `waker` when others start or end notes.
    pub fn with_waker(mut self, waker: crate::link::Waker) -> Self {
        self.waker = Some(waker);
        self
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sets the track this player's notes go to. Notes that sound stay where they started.
    pub fn set_target(&self, track: Option<TrackId>) {
        self.state().target = track;
    }

    pub fn target(&self) -> Option<TrackId> {
        self.state().target
    }

    /// Starts a note on the target track, if there is one.
    pub fn press(&self, pitch: u8, vel: u8) {
        let mut state = self.state();
        let Some(track) = state.target else {
            return;
        };
        let pitch = pitch.min(127);
        if state.held.contains_key(&pitch) {
            self.end(&mut state, pitch);
        }
        let now = clock::now_micros();
        let vel = vel.clamp(1, 127);
        self.engine.note_on(track, OWN_KEY | pitch as u64, pitch, vel);
        state.held.insert(pitch, track);
        let packet = state.out.play(track, pitch, vel, now);
        state.send(packet);
        state.capture(now, track, pitch, vel);
    }

    /// Ends a note this player started.
    pub fn release(&self, pitch: u8) {
        let mut state = self.state();
        self.end(&mut state, pitch);
    }

    /// Ends every note this player holds.
    pub fn release_all(&self) {
        let mut state = self.state();
        let pitches: Vec<u8> = state.held.keys().copied().collect();
        for pitch in pitches {
            self.end(&mut state, pitch);
        }
    }

    fn end(&self, state: &mut State, pitch: u8) {
        let Some(track) = state.held.remove(&pitch) else {
            return;
        };
        let now = clock::now_micros();
        self.engine.note_off(track, OWN_KEY | pitch as u64);
        let packet = state.out.play(track, pitch, 0, now);
        state.send(packet);
        state.capture(now, track, pitch, 0);
    }

    /// Plays what another player sent.
    pub fn receive(&self, packet: &MidiPacket) {
        let mut state = self.state();
        if state.deaf.contains(&packet.track) || !state.present.contains(&packet.actor) {
            return;
        }
        let actions = state.inn.receive(packet, clock::now_micros());
        self.perform(actions);
    }

    /// Keeps the others' picture of this player's notes fresh and silences players not heard
    /// from. Call every [`TICK`].
    pub fn tick(&self) {
        let now = clock::now_micros();
        let mut state = self.state();
        for packet in state.out.tick(now) {
            state.send(packet);
        }
        let actions = state.inn.tick(now);
        self.perform(actions);
    }

    /// The connection notes go out on. Without one, the others' notes end.
    pub fn set_connection(&self, connection: Option<quinn::Connection>) {
        let mut state = self.state();
        if connection.is_none() {
            let actions = state.inn.silence_all();
            self.perform(actions);
        }
        state.connection = connection;
    }

    /// Who is there. The notes of anyone else end.
    pub fn set_present(&self, actors: BTreeSet<ActorId>) {
        let mut state = self.state();
        if state.present == actors {
            return;
        }
        let gone: Vec<ActorId> = state.present.difference(&actors).copied().collect();
        for gone in gone {
            let actions = state.inn.silence_actor(gone);
            self.perform(actions);
        }
        state.present = actors;
    }

    /// The tracks that do not hear the others. What the others play there ends.
    pub fn set_deaf(&self, tracks: BTreeSet<TrackId>) {
        let mut state = self.state();
        if state.deaf == tracks {
            return;
        }
        let newly: Vec<TrackId> = tracks.difference(&state.deaf).copied().collect();
        for track in newly {
            let actions = state.inn.silence_track(track);
            self.perform(actions);
        }
        state.deaf = tracks;
    }

    /// Starts or stops keeping this player's notes for [`Live::take_played`].
    pub fn set_capturing(&self, capturing: bool) {
        let mut state = self.state();
        state.capturing = capturing;
        if !capturing {
            state.played.clear();
        }
    }

    /// The notes played since the last call, while capturing.
    pub fn take_played(&self) -> Vec<Played> {
        std::mem::take(&mut self.state().played)
    }

    /// This player's notes that sound, with their track.
    pub fn held(&self) -> Vec<(TrackId, u8)> {
        self.state()
            .held
            .iter()
            .map(|(&pitch, &track)| (track, pitch))
            .collect()
    }

    /// Who else plays on `track` right now.
    pub fn players(&self, track: TrackId) -> Vec<ActorId> {
        self.state().inn.players(track)
    }

    /// How many notes of others sound here.
    pub fn others_sounding(&self) -> usize {
        self.state().inn.sounding()
    }

    fn perform(&self, actions: Vec<LiveAction>) {
        if let Some(waker) = self.waker.as_ref().filter(|_| !actions.is_empty()) {
            waker();
        }
        for action in actions {
            match action {
                LiveAction::On {
                    actor,
                    track,
                    pitch,
                    vel,
                } => {
                    self.engine.note_on(track, remote_key(actor, pitch), pitch, vel);
                }
                LiveAction::Off { actor, track, pitch } => {
                    self.engine.note_off(track, remote_key(actor, pitch));
                }
            }
        }
    }
}

impl State {
    fn send(&self, packet: MidiPacket) {
        if let Some(connection) = &self.connection {
            mdaw_net::send_datagram(connection, &Datagram::Midi(packet));
        }
    }

    fn capture(&mut self, micros: i64, track: TrackId, pitch: u8, vel: u8) {
        if self.capturing {
            self.played.push(Played {
                micros,
                track,
                pitch,
                vel,
            });
        }
    }
}

fn remote_key(actor: ActorId, pitch: u8) -> u64 {
    REMOTE_KEY | (actor.0 as u64) << 8 | pitch as u64
}

/// Keeps a MIDI device open on a thread of its own: the one the user picked, whenever it is
/// plugged in. Stops when dropped.
pub struct MidiWatch {
    shared: Arc<WatchShared>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct WatchShared {
    stop: AtomicBool,
    /// The device the user picked.
    wanted: Mutex<Option<String>>,
    /// The inputs there are, and the one that is open.
    seen: Mutex<(Vec<String>, Option<String>)>,
}

impl MidiWatch {
    const EVERY: Duration = Duration::from_secs(1);

    pub fn start(live: Live) -> std::io::Result<Self> {
        let shared = Arc::new(WatchShared::default());
        let thread = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("mdaw-midi".into())
                .spawn(move || watch(shared, live))?
        };
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Picks the device to listen to, or none.
    pub fn set_wanted(&self, name: Option<String>) {
        *self.shared.wanted.lock().unwrap_or_else(|e| e.into_inner()) = name;
    }

    pub fn wanted(&self) -> Option<String> {
        self.shared
            .wanted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The inputs there are, as of the last look.
    pub fn devices(&self) -> Vec<String> {
        self.shared
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .0
            .clone()
    }

    /// The device that is open.
    pub fn open(&self) -> Option<String> {
        self.shared
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .1
            .clone()
    }
}

impl Drop for MidiWatch {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn watch(shared: Arc<WatchShared>, live: Live) {
    let mut open: Option<MidiDevice> = None;
    while !shared.stop.load(Ordering::Relaxed) {
        let devices = midi::devices();
        let wanted = shared.wanted.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let keep = open.as_ref().is_some_and(|d| {
            wanted.as_deref().is_some_and(|w| midi::same_device(w, d.name()))
                && devices.iter().any(|n| n == d.name())
        });
        if !keep && open.take().is_some() {
            // Unplugged or no longer wanted: what it held would never end.
            live.release_all();
        }
        if open.is_none()
            && let Some(name) =
                wanted.and_then(|w| devices.iter().find(|n| midi::same_device(n, &w)).cloned())
        {
            let live = live.clone();
            open = MidiDevice::open(&name, move |msg| match msg {
                MidiMsg::On { pitch, vel } => live.press(pitch, vel),
                MidiMsg::Off { pitch } => live.release(pitch),
                MidiMsg::AllOff => live.release_all(),
            })
            .ok();
        }
        *shared.seen.lock().unwrap_or_else(|e| e.into_inner()) =
            (devices, open.as_ref().map(|d| d.name().to_string()));
        let mut slept = Duration::ZERO;
        while slept < MidiWatch::EVERY && !shared.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
            slept += Duration::from_millis(50);
        }
    }
}
