//! Live notes: what this player plays on the computer keyboard or a MIDI device, heard here at
//! once and kept for recording. Nothing of it goes over the network; a recording becomes an
//! ordinary edit.
//!
//! [`Live`] is shared by the UI and the thread of a MIDI device, so a note never waits for a
//! frame of the UI. The UI only decides which track notes go to.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use mdaw_engine::midi::{self, MidiDevice, MidiMsg};
use mdaw_engine::{LiveInput, clock};
use mdaw_model::TrackId;

/// Engine keys of this player's notes: the pitch is added.
const OWN_KEY: u64 = 1 << 40;

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
}

#[derive(Default)]
struct State {
    /// The track notes go to.
    target: Option<TrackId>,
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
        }
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
        state.capture(now, track, pitch, 0);
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
}

impl State {
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
