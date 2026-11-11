//! Passes live notes from one player to the others, on the network threads: they never wait
//! for the project thread.

use mdaw_model::{ActorId, Project, TrackId, UserId};
use mdaw_proto::{Datagram, MidiPacket};
use quinn::Connection;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Clone, Default)]
pub struct Relay {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Default)]
struct Inner {
    players: HashMap<ActorId, (UserId, Connection)>,
    /// Who has each checked-out track.
    checkouts: HashMap<TrackId, UserId>,
}

impl Relay {
    pub fn add(&self, actor: ActorId, user: UserId, connection: Connection) {
        self.write().players.insert(actor, (user, connection));
    }

    pub fn remove(&self, actor: ActorId) {
        self.write().players.remove(&actor);
    }

    /// Takes the checkouts from the document.
    pub fn set_checkouts(&self, doc: &Project) {
        let checkouts = doc
            .tracks
            .iter()
            .filter_map(|(&id, track)| Some((id, track.checkout?.user)))
            .collect();
        self.write().checkouts = checkouts;
    }

    /// Sends `packet` from `actor` to everyone else. A track someone has checked out hears
    /// nobody but its owner in that owner's window, until the commit.
    pub fn forward(&self, actor: ActorId, mut packet: MidiPacket) {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let Some(&(sender, _)) = inner.players.get(&actor) else {
            return;
        };
        packet.actor = actor;
        let owner = inner.checkouts.get(&packet.track).copied();
        let bytes = Datagram::Midi(packet).encode();
        for (&to, (user, connection)) in &inner.players {
            let deaf = owner == Some(*user) && sender != *user;
            if to != actor && !deaf {
                let _ = connection.send_datagram(bytes.clone().into());
            }
        }
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }
}
