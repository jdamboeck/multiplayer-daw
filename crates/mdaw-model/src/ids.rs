use serde::{Deserialize, Serialize};
use std::fmt;

/// One connection to the server. The server never hands out the same value twice, so IDs minted
/// under different actors cannot collide.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct ActorId(pub u32);

/// One client install. Survives reconnects; checkouts are owned by a user, not an actor.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct UserId(pub u32);

/// `actor << 32 | counter`. Minted by clients without coordination.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EntityId(pub u64);

impl EntityId {
    pub const fn new(actor: ActorId, counter: u32) -> Self {
        Self(((actor.0 as u64) << 32) | counter as u64)
    }
    pub const fn actor(self) -> ActorId {
        ActorId((self.0 >> 32) as u32)
    }
    pub const fn counter(self) -> u32 {
        self.0 as u32
    }
}

impl fmt::Debug for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.actor().0, self.counter())
    }
}

macro_rules! typed_id {
    ($($(#[$meta:meta])* $name:ident => $tag:literal),* $(,)?) => {$(
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub struct $name(pub EntityId);

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($tag, "{:?}"), self.0)
            }
        }
    )*};
}

typed_id! {
    TrackId => "track:",
    ClipId => "clip:",
    NoteId => "note:",
    DeviceId => "device:",
    TempoId => "tempo:",
}

/// Mints entity IDs for one actor.
#[derive(Clone, Debug)]
pub struct IdGen {
    actor: ActorId,
    next: u32,
}

impl IdGen {
    pub fn new(actor: ActorId) -> Self {
        Self { actor, next: 1 }
    }
    pub fn actor(&self) -> ActorId {
        self.actor
    }
    pub fn entity(&mut self) -> EntityId {
        let id = EntityId::new(self.actor, self.next);
        self.next = self.next.checked_add(1).expect("an actor ran out of entity ids");
        id
    }
    pub fn track(&mut self) -> TrackId {
        TrackId(self.entity())
    }
    pub fn clip(&mut self) -> ClipId {
        ClipId(self.entity())
    }
    pub fn note(&mut self) -> NoteId {
        NoteId(self.entity())
    }
    pub fn device(&mut self) -> DeviceId {
        DeviceId(self.entity())
    }
    pub fn tempo(&mut self) -> TempoId {
        TempoId(self.entity())
    }
}

/// BLAKE3 hash of a blob in the content-addressed store.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BlobHash(pub [u8; 32]);

impl fmt::Debug for BlobHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "blob:")?;
        self.0[..6].iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl fmt::Display for BlobHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}
