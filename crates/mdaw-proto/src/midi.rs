//! Live notes between players: what travels in [`MidiPacket`] datagrams, and the two ends of
//! a stream of them.
//!
//! Datagrams get lost and arrive out of order, so every packet repeats the last few events of
//! its stream and lists every note the player still holds. A receiver ends any note it plays
//! that the list no longer has, and everything of a stream it has not heard from for
//! [`TIMEOUT_MICROS`]. While notes are held the sender repeats itself every
//! [`KEEPALIVE_MICROS`], and for [`TAIL_MICROS`] after the last one ended, so a release that
//! got lost is made up for soon.
//!
//! Time is microseconds on any clock that only moves forward; each end uses its own.

use mdaw_model::{ActorId, TrackId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// How often a sender repeats itself while notes are held.
pub const KEEPALIVE_MICROS: i64 = 250_000;
/// A stream not heard from for this long is silenced.
pub const TIMEOUT_MICROS: i64 = 2_000_000;
/// How long a sender goes on repeating itself after its last note ended.
pub const TAIL_MICROS: i64 = 1_000_000;
/// Events a packet repeats besides the newest.
pub const REPEATED: usize = 2;
/// Pitches are MIDI note numbers, below this.
pub const PITCHES: u8 = 128;

/// One player's notes on one track, as of one moment.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct MidiPacket {
    /// Who plays. Set by the server when it passes the packet on; what a client says is
    /// ignored.
    pub actor: ActorId,
    pub track: TrackId,
    /// Counts the packets of this player's stream on this track.
    pub stream_seq: u32,
    /// The newest events of the stream, oldest first, at most `REPEATED + 1`.
    pub events: Vec<LiveEvent>,
    /// Every pitch the player holds on the track after these events.
    pub held: Vec<u8>,
}

/// A note starting or ending.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct LiveEvent {
    /// Counts the events of the stream.
    pub seq: u32,
    pub pitch: u8,
    /// 1 to 127 starts the note, 0 ends it.
    pub vel: u8,
}

/// The sending end: the notes this player holds, by track.
#[derive(Default)]
pub struct MidiOut {
    streams: BTreeMap<TrackId, OutStream>,
}

#[derive(Default)]
struct OutStream {
    packets: u32,
    events: u32,
    recent: VecDeque<LiveEvent>,
    held: BTreeSet<u8>,
    sent_at: i64,
    /// When the last held note ended. `None` while notes are held.
    quiet_since: Option<i64>,
}

impl MidiOut {
    pub fn new() -> Self {
        Self::default()
    }

    /// A note starts (`vel > 0`) or ends (`vel == 0`) on `track`. Returns the packet that says so.
    pub fn play(&mut self, track: TrackId, pitch: u8, vel: u8, now: i64) -> MidiPacket {
        let stream = self.streams.entry(track).or_default();
        stream.events += 1;
        let event = LiveEvent {
            seq: stream.events,
            pitch: pitch.min(PITCHES - 1),
            vel: vel.min(127),
        };
        if event.vel > 0 {
            stream.held.insert(event.pitch);
        } else {
            stream.held.remove(&event.pitch);
        }
        stream.recent.push_back(event);
        if stream.recent.len() > REPEATED + 1 {
            stream.recent.pop_front();
        }
        stream.quiet_since = stream.held.is_empty().then_some(now);
        stream.packet(track, now)
    }

    /// The packets that are due to keep the receivers' picture fresh.
    pub fn tick(&mut self, now: i64) -> Vec<MidiPacket> {
        let mut due = Vec::new();
        for (&track, stream) in &mut self.streams {
            let active = stream.quiet_since.is_none_or(|quiet| now - quiet < TAIL_MICROS);
            if active && now - stream.sent_at >= KEEPALIVE_MICROS {
                due.push(stream.packet(track, now));
            }
        }
        due
    }

    /// The pitches held on `track`.
    pub fn held(&self, track: TrackId) -> impl Iterator<Item = u8> + '_ {
        self.streams
            .get(&track)
            .into_iter()
            .flat_map(|s| s.held.iter().copied())
    }

    /// Every track with notes held, and the pitch.
    pub fn all_held(&self) -> Vec<(TrackId, u8)> {
        self.streams
            .iter()
            .flat_map(|(&track, s)| s.held.iter().map(move |&pitch| (track, pitch)))
            .collect()
    }
}

impl OutStream {
    fn packet(&mut self, track: TrackId, now: i64) -> MidiPacket {
        self.packets += 1;
        self.sent_at = now;
        MidiPacket {
            actor: ActorId(0),
            track,
            stream_seq: self.packets,
            events: self.recent.iter().copied().collect(),
            held: self.held.iter().copied().collect(),
        }
    }
}

/// What a receiver does to its instrument.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LiveAction {
    On {
        actor: ActorId,
        track: TrackId,
        pitch: u8,
        vel: u8,
    },
    Off {
        actor: ActorId,
        track: TrackId,
        pitch: u8,
    },
}

/// The receiving end: the notes of other players that sound here.
#[derive(Default)]
pub struct MidiIn {
    streams: BTreeMap<(ActorId, TrackId), InStream>,
}

struct InStream {
    packet: u32,
    event: u32,
    sounding: BTreeSet<u8>,
    heard_at: i64,
}

impl MidiIn {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes in a packet. Returns what to start and end, in that order.
    pub fn receive(&mut self, packet: &MidiPacket, now: i64) -> Vec<LiveAction> {
        let (actor, track) = (packet.actor, packet.track);
        let held: BTreeSet<u8> = packet.held.iter().copied().filter(|&p| p < PITCHES).collect();
        let stream = self.streams.entry((actor, track)).or_insert(InStream {
            packet: 0,
            event: 0,
            sounding: BTreeSet::new(),
            heard_at: now,
        });
        // Older than what is known already, or the same again.
        if packet.stream_seq <= stream.packet {
            return Vec::new();
        }
        stream.packet = packet.stream_seq;
        stream.heard_at = now;
        let mut events: Vec<LiveEvent> = packet
            .events
            .iter()
            .copied()
            .filter(|e| e.pitch < PITCHES && e.seq > stream.event)
            .collect();
        events.sort_by_key(|e| e.seq);
        let mut actions = Vec::new();
        let off = |pitch| LiveAction::Off { actor, track, pitch };
        for event in events {
            stream.event = event.seq;
            if event.vel == 0 {
                if stream.sounding.remove(&event.pitch) {
                    actions.push(off(event.pitch));
                }
            } else if held.contains(&event.pitch) {
                // A note that is not held any more ended in an event this packet no longer
                // carries: starting it would only cut it off again.
                if !stream.sounding.insert(event.pitch) {
                    actions.push(off(event.pitch));
                }
                actions.push(LiveAction::On {
                    actor,
                    track,
                    pitch: event.pitch,
                    vel: event.vel.min(127),
                });
            }
        }
        let ended: Vec<u8> = stream.sounding.difference(&held).copied().collect();
        for pitch in ended {
            stream.sounding.remove(&pitch);
            actions.push(off(pitch));
        }
        actions
    }

    /// Silences the streams not heard from for too long.
    pub fn tick(&mut self, now: i64) -> Vec<LiveAction> {
        self.silence(|_, _, heard_at| now - heard_at > TIMEOUT_MICROS)
    }

    /// Silences and forgets the streams of `actor`, as when that player left.
    pub fn silence_actor(&mut self, actor: ActorId) -> Vec<LiveAction> {
        self.silence(|a, _, _| a == actor)
    }

    /// Silences and forgets the streams on `track`.
    pub fn silence_track(&mut self, track: TrackId) -> Vec<LiveAction> {
        self.silence(|_, t, _| t == track)
    }

    /// Silences and forgets everything.
    pub fn silence_all(&mut self) -> Vec<LiveAction> {
        self.silence(|_, _, _| true)
    }

    fn silence(&mut self, gone: impl Fn(ActorId, TrackId, i64) -> bool) -> Vec<LiveAction> {
        let mut actions = Vec::new();
        self.streams.retain(|&(actor, track), stream| {
            if !gone(actor, track, stream.heard_at) {
                return true;
            }
            actions.extend(
                stream
                    .sounding
                    .iter()
                    .map(|&pitch| LiveAction::Off { actor, track, pitch }),
            );
            false
        });
        actions
    }

    /// Notes of others sounding here.
    pub fn sounding(&self) -> usize {
        self.streams.values().map(|s| s.sounding.len()).sum()
    }

    /// Who is playing on `track` right now.
    pub fn players(&self, track: TrackId) -> Vec<ActorId> {
        let mut players: Vec<ActorId> = self
            .streams
            .iter()
            .filter(|((_, t), s)| *t == track && !s.sounding.is_empty())
            .map(|((a, _), _)| *a)
            .collect();
        players.dedup();
        players
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const TRACK: TrackId = TrackId(mdaw_model::EntityId(7));
    const ANN: ActorId = ActorId(3);
    const MS: i64 = 1000;

    /// What the receiver's instrument plays, as it follows the actions.
    #[derive(Default)]
    struct Instrument {
        sounding: BTreeMap<(ActorId, TrackId, u8), u8>,
        /// Starts of a note that already sounded, without an end in between.
        doubled: usize,
    }

    impl Instrument {
        fn apply(&mut self, actions: Vec<LiveAction>) {
            for action in actions {
                match action {
                    LiveAction::On {
                        actor,
                        track,
                        pitch,
                        vel,
                    } => {
                        if self.sounding.insert((actor, track, pitch), vel).is_some() {
                            self.doubled += 1;
                        }
                    }
                    LiveAction::Off { actor, track, pitch } => {
                        self.sounding.remove(&(actor, track, pitch));
                    }
                }
            }
        }

        fn pitches(&self) -> BTreeSet<u8> {
            self.sounding.keys().map(|k| k.2).collect()
        }
    }

    fn from_ann(mut packet: MidiPacket) -> MidiPacket {
        packet.actor = ANN;
        packet
    }

    /// A small deterministic random source.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn chance(&mut self, percent: u64) -> bool {
            self.next() % 100 < percent
        }
    }

    #[test]
    fn without_loss_the_receiver_plays_what_the_sender_holds() {
        let (mut out, mut inn, mut synth) = (MidiOut::new(), MidiIn::new(), Instrument::default());
        let mut rng = Rng(42);
        for step in 0..2000 {
            let now = step * 10 * MS;
            let pitch = 60 + (rng.next() % 12) as u8;
            let vel = if out.held(TRACK).any(|p| p == pitch) {
                0
            } else {
                100
            };
            synth.apply(inn.receive(&from_ann(out.play(TRACK, pitch, vel, now)), now));
            assert_eq!(synth.pitches(), out.held(TRACK).collect());
            for packet in out.tick(now) {
                synth.apply(inn.receive(&from_ann(packet), now));
            }
            assert_eq!(synth.doubled, 0);
        }
    }

    #[test]
    fn a_lost_release_is_made_up_for_by_the_next_packet() {
        let (mut out, mut inn, mut synth) = (MidiOut::new(), MidiIn::new(), Instrument::default());
        synth.apply(inn.receive(&from_ann(out.play(TRACK, 60, 100, 0)), 0));
        assert_eq!(synth.pitches(), [60].into());
        let _lost = out.play(TRACK, 60, 0, 10 * MS);
        // Nothing else is played: the keepalive after the release carries the news.
        assert!(out.tick(100 * MS).is_empty());
        let keepalive = out.tick(260 * MS);
        assert_eq!(keepalive.len(), 1);
        synth.apply(inn.receive(&from_ann(keepalive[0].clone()), 260 * MS));
        assert!(synth.pitches().is_empty());
        // The keepalives stop a while after the last note ended.
        assert!(out.tick(2 * TAIL_MICROS).is_empty());
    }

    #[test]
    fn a_lost_start_is_made_up_for_by_the_next_event() {
        let (mut out, mut inn, mut synth) = (MidiOut::new(), MidiIn::new(), Instrument::default());
        let _lost = out.play(TRACK, 60, 100, 0);
        synth.apply(inn.receive(&from_ann(out.play(TRACK, 64, 90, MS)), MS));
        assert_eq!(synth.pitches(), [60, 64].into());
    }

    #[test]
    fn a_note_that_already_ended_is_not_started_late() {
        let (mut out, mut inn, mut synth) = (MidiOut::new(), MidiIn::new(), Instrument::default());
        let _lost = out.play(TRACK, 60, 100, 0);
        let _lost = out.play(TRACK, 60, 0, MS);
        let mut actions = inn.receive(&from_ann(out.play(TRACK, 64, 90, 2 * MS)), 2 * MS);
        assert_eq!(actions.len(), 1);
        synth.apply(std::mem::take(&mut actions));
        assert_eq!(synth.pitches(), [64].into());
    }

    #[test]
    fn late_and_repeated_packets_change_nothing() {
        let (mut out, mut inn, mut synth) = (MidiOut::new(), MidiIn::new(), Instrument::default());
        let start = from_ann(out.play(TRACK, 60, 100, 0));
        let release = from_ann(out.play(TRACK, 60, 0, MS));
        synth.apply(inn.receive(&release, MS));
        assert!(inn.receive(&start, 2 * MS).is_empty());
        assert!(inn.receive(&release, 2 * MS).is_empty());
        assert!(synth.pitches().is_empty());
    }

    #[test]
    fn a_player_not_heard_from_is_silenced() {
        let (mut out, mut inn, mut synth) = (MidiOut::new(), MidiIn::new(), Instrument::default());
        synth.apply(inn.receive(&from_ann(out.play(TRACK, 60, 100, 0)), 0));
        assert_eq!(inn.players(TRACK), [ANN]);
        synth.apply(inn.tick(TIMEOUT_MICROS));
        assert_eq!(synth.pitches(), [60].into());
        synth.apply(inn.tick(TIMEOUT_MICROS + MS));
        assert!(synth.pitches().is_empty());
        assert_eq!(inn.sounding(), 0);
    }

    #[test]
    fn under_heavy_loss_and_reordering_no_note_is_left_hanging() {
        for seed in 1..40 {
            let (mut out, mut inn, mut synth) = (MidiOut::new(), MidiIn::new(), Instrument::default());
            let mut rng = Rng(seed * 7919);
            // Packets on their way, with the time they arrive.
            let mut wire: Vec<(i64, MidiPacket)> = Vec::new();
            let send = |rng: &mut Rng, wire: &mut Vec<(i64, MidiPacket)>, packet, now: i64| {
                if !rng.chance(20) {
                    let delay = 80 * MS + (rng.next() % 40) as i64 * MS;
                    wire.push((now + delay, from_ann(packet)));
                }
            };
            let mut now = 0;
            for _ in 0..500 {
                now += (rng.next() % 30) as i64 * MS;
                let pitch = 48 + (rng.next() % 24) as u8;
                let vel = if out.held(TRACK).any(|p| p == pitch) {
                    0
                } else {
                    1 + (rng.next() % 127) as u8
                };
                let packet = out.play(TRACK, pitch, vel, now);
                send(&mut rng, &mut wire, packet, now);
                for packet in out.tick(now) {
                    send(&mut rng, &mut wire, packet, now);
                }
                wire.sort_by_key(|(at, _)| *at);
                let arrived = wire.partition_point(|(at, _)| *at <= now);
                for (_, packet) in wire.drain(..arrived) {
                    synth.apply(inn.receive(&packet, now));
                }
                synth.apply(inn.tick(now));
                // What sounds was held at some point: never a note the player does not play.
                assert!(synth.pitches().len() <= 24);
            }
            // The player lets go of everything.
            for (track, pitch) in out.all_held() {
                let packet = out.play(track, pitch, 0, now);
                send(&mut rng, &mut wire, packet, now);
            }
            let end = now;
            while now < end + TIMEOUT_MICROS {
                now += 10 * MS;
                for packet in out.tick(now) {
                    send(&mut rng, &mut wire, packet, now);
                }
                wire.sort_by_key(|(at, _)| *at);
                let arrived = wire.partition_point(|(at, _)| *at <= now);
                for (_, packet) in wire.drain(..arrived) {
                    synth.apply(inn.receive(&packet, now));
                }
                synth.apply(inn.tick(now));
                if synth.pitches().is_empty() {
                    break;
                }
            }
            assert!(synth.pitches().is_empty(), "seed {seed}: notes left hanging");
            assert_eq!(synth.doubled, 0, "seed {seed}");
            // With a fifth of the packets lost, the silence came long before the timeout.
            assert!(
                now - end < TIMEOUT_MICROS,
                "seed {seed}: took {} ms",
                (now - end) / MS
            );
        }
    }

    #[test]
    fn packets_round_trip_and_stay_small() {
        let mut out = MidiOut::new();
        for pitch in 0..PITCHES {
            out.play(TRACK, pitch, 100, 0);
        }
        let packet = crate::Datagram::Midi(from_ann(out.play(TRACK, 0, 0, 0)));
        let bytes = packet.encode();
        // Fits a datagram on any path QUIC runs on, with every note held.
        assert!(bytes.len() < 1200, "{} bytes", bytes.len());
        assert_eq!(crate::Datagram::decode(&bytes).unwrap(), packet);
    }
}
