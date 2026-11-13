use mdaw_model::{ActorId, UserId};
use mdaw_proto::{Event, Txn};
use std::time::{SystemTime, UNIX_EPOCH};

/// Stands in for the server when working alone: gives every transaction the next sequence
/// number and hands it straight back.
pub struct Loopback {
    seq: u64,
    user: UserId,
}

impl Loopback {
    pub fn new(head_seq: u64, user: UserId) -> Self {
        Self { seq: head_seq, user }
    }

    pub fn head_seq(&self) -> u64 {
        self.seq
    }

    pub fn sequence(&mut self, actor: ActorId, txn: Txn) -> Event {
        self.seq += 1;
        let wall_ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Event {
            seq: self.seq,
            actor,
            user: self.user,
            wall_ts,
            client_txn_id: txn.client_txn_id,
            ops: txn.ops,
            superseded: Vec::new(),
        }
    }
}
