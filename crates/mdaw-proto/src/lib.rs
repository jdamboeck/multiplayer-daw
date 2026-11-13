//! What travels between client and server. The document itself lives in `mdaw-model`; this crate
//! wraps its ops into transactions and into the events of the project's log.

mod wire;

use mdaw_model::{ActorId, BlobHash, EntityRef, Op, Project, PropKey, UserId, apply};
use serde::{Deserialize, Serialize};

pub use wire::{
    ALPN, BlobAnswer, BlobRequest, ClientMsg, FrameError, Framed, Hello, MAX_BLOB, MAX_FRAME,
    MAX_SERVER_FRAME, Peer, Presence, RefuseReason, RejectReason, Resume, ServerMsg, SyncData, Welcome,
    decode, encode, payload_len,
};

/// Bumped on every incompatible change to the wire format or to the meaning of an op.
pub const PROTO_VERSION: u32 = 6;

/// The hash a blob of these bytes is stored and named under.
pub fn blob_hash(bytes: &[u8]) -> BlobHash {
    BlobHash(*blake3::hash(bytes).as_bytes())
}

/// One user gesture: applied atomically, sequenced as a whole, undone as a whole.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Txn {
    /// Chosen by the client, increasing per connection. Echoed in the event so the client can
    /// match it with its pending transaction.
    pub client_txn_id: u64,
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
    /// The sender's [`Txn::client_txn_id`], so it can match the event with its pending
    /// transaction.
    pub client_txn_id: u64,
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
