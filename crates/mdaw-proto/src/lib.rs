//! What travels between client and server. The document itself lives in `mdaw-model`; this crate
//! wraps its ops into transactions and into the events of the project's log.

mod wire;

use mdaw_model::{ActorId, EntityRef, Op, PropKey, UserId};
use serde::{Deserialize, Serialize};

pub use wire::{
    ALPN, ClientMsg, FrameError, Hello, MAX_FRAME, Peer, Presence, RefuseReason, RejectReason, Resume,
    ServerMsg, SyncData, Welcome, decode, encode, payload_len,
};

/// Bumped on every incompatible change to the wire format or to the meaning of an op.
pub const PROTO_VERSION: u32 = 1;

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

/// One entry of a project's totally ordered log. The document is the fold of all events; the
/// changelog is a view of the same log.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Event {
    /// Position in the log, starting at 1 and without gaps.
    pub seq: u64,
    pub actor: ActorId,
    pub user: UserId,
    /// Server wall clock, milliseconds since the Unix epoch. For display only.
    pub wall_ts: u64,
    pub body: EventBody,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum EventBody {
    /// A live edit.
    Txn {
        client_txn_id: u64,
        ops: Vec<Op>,
        /// Concurrent edits by others that this one displaced.
        superseded: Vec<Superseded>,
    },
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
    /// The sender's transaction ID, if this event came from a `Submit`.
    pub fn client_txn_id(&self) -> Option<u64> {
        match &self.body {
            EventBody::Txn { client_txn_id, .. } => Some(*client_txn_id),
        }
    }

    /// The document ops this event carries, in application order.
    pub fn ops(&self) -> &[Op] {
        match &self.body {
            EventBody::Txn { ops, .. } => ops,
        }
    }
}
