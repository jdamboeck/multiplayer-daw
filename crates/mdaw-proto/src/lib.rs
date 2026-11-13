//! What travels between client and server. The document itself lives in `mdaw-model`; this crate
//! wraps its ops into transactions and into the events of the project's log.

mod wire;

use mdaw_model::{ActorId, BlobHash, EntityRef, Op, Project, PropKey, UserId, apply};
use serde::{Deserialize, Serialize};
use std::fmt;

pub use wire::{
    ALPN, BlobAnswer, BlobRequest, ClientMsg, FrameError, Framed, Hello, MAX_BLOB, MAX_FRAME,
    MAX_SERVER_FRAME, Peer, Presence, RefuseReason, RejectReason, Resume, ServerMsg, SyncData, Welcome,
    decode, encode, payload_len,
};

/// Bumped on every incompatible change to the wire format or to the meaning of an op.
pub const PROTO_VERSION: u32 = 8;

/// The hash a blob of these bytes is stored and named under.
pub fn blob_hash(bytes: &[u8]) -> BlobHash {
    BlobHash(*blake3::hash(bytes).as_bytes())
}

/// The identity of a transaction: 128 random bits, chosen once by the client that makes it and
/// never changed. It stays with the pending transaction across reconnects and restarts of the
/// client, so the server can tell a transaction sent again from a new one (see the receipts in
/// `docs/protocol.md`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TxnId(pub [u8; 16]);

impl TxnId {
    /// A new identity from the system's random numbers.
    pub fn random() -> Self {
        let mut bytes = [0u8; 16];
        // Without random numbers from the system no identity is safe to hand out, and nothing
        // else in the process would work either.
        getrandom::fill(&mut bytes).expect("random numbers from the system");
        Self(bytes)
    }
}

/// For tests and tools that want identities they can name.
impl From<u128> for TxnId {
    fn from(n: u128) -> Self {
        Self(n.to_be_bytes())
    }
}

impl fmt::Display for TxnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for TxnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TxnId({self})")
    }
}

/// One user gesture: applied atomically, sequenced as a whole, undone as a whole.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Txn {
    /// Chosen by the client when it makes the transaction, and sent unchanged however often the
    /// transaction is sent. Echoed in the event so the client can match it with its pending
    /// transaction.
    pub id: TxnId,
    /// The last server sequence number the client had applied when it made the edit. Events
    /// after it are what the edit could not have known about.
    pub base_seq: u64,
    pub ops: Vec<Op>,
}

/// One entry of a project's totally ordered log: a transaction as the server sequenced it. The
/// document is the fold of all events.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Event {
    /// Position in the log, starting at 1 and without gaps.
    pub seq: u64,
    pub actor: ActorId,
    pub user: UserId,
    /// Server wall clock, milliseconds since the Unix epoch. For display only.
    pub wall_ts: u64,
    /// The sender's [`Txn::id`], so it can match the event with its pending transaction.
    pub txn: TxnId,
    pub ops: Vec<Op>,
    /// Concurrent edits by others that this one displaced.
    pub superseded: Vec<Superseded>,
}

/// A write that lost against a later one it did not know about. Kept so it can be restored.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Superseded {
    pub entity: EntityRef,
    pub prop: PropKey,
    /// The displaced op. Submitting it again restores the lost edit.
    pub lost: Op,
    pub lost_actor: ActorId,
    pub lost_user: UserId,
    pub lost_seq: u64,
}

impl Event {
    /// True for events the history shows: those that displaced someone's edit.
    pub fn is_notable(&self) -> bool {
        !self.superseded.is_empty()
    }
}

/// Folds one event into the document. Server and clients build their state with nothing else,
/// so they agree on it.
pub fn apply_event(project: &mut Project, event: &Event) {
    for op in &event.ops {
        apply(project, op);
    }
}
