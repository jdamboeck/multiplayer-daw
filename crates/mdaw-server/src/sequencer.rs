//! Decides what enters the log and in which order. Pure: no I/O, no clock.

use mdaw_model::{ActorId, Op, Project, PropKey, Scope, UserId, apply};
use mdaw_proto::{Event, EventBody, RejectReason, Superseded, Txn};
use std::collections::VecDeque;

/// Events kept in memory to find what a transaction displaced. A client further behind than
/// this gets fewer `Superseded` records, nothing else.
const TAIL: usize = 4096;

/// The server's copy of the document and the end of its log.
pub struct Sequencer {
    doc: Project,
    head: u64,
    tail: VecDeque<Event>,
}

impl Sequencer {
    /// `doc` is the document after event `head`. `recent` are the last events up to `head`, as
    /// many as are at hand.
    pub fn new(doc: Project, head: u64, recent: Vec<Event>) -> Self {
        debug_assert!(recent.last().is_none_or(|e| e.seq == head));
        let mut tail = VecDeque::from(recent);
        while tail.len() > TAIL {
            tail.pop_front();
        }
        Self { doc, head, tail }
    }

    pub fn doc(&self) -> &Project {
        &self.doc
    }

    /// Sequence number of the last event.
    pub fn head(&self) -> u64 {
        self.head
    }

    /// The events after `seq`, if memory still holds all of them.
    pub fn events_after(&self, seq: u64) -> Option<Vec<Event>> {
        if seq >= self.head {
            return Some(Vec::new());
        }
        let first = self.tail.front()?.seq;
        (first <= seq + 1).then(|| self.tail.iter().filter(|e| e.seq > seq).cloned().collect())
    }

    /// Checks that would refuse `txn`, without changing anything.
    pub fn check(&self, user: UserId, txn: &Txn) -> Result<(), RejectReason> {
        if txn.ops.is_empty() {
            return Err(RejectReason::Invalid("empty transaction".into()));
        }
        if txn.base_seq > self.head {
            return Err(RejectReason::FutureBase);
        }
        for op in &txn.ops {
            op.validate().map_err(|e| RejectReason::Invalid(e.to_string()))?;
            if op.adds_clap_device() {
                return Err(RejectReason::ClapOutsideCommit);
            }
            match op.scope() {
                Scope::Project => {}
                Scope::TrackContent(id) => {
                    // A track this same transaction adds is not in the document yet, and is free.
                    let Some(track) = self.doc.track(id) else { continue };
                    if let Some(checkout) = track.checkout {
                        return Err(RejectReason::CheckedOut {
                            track: id,
                            by: checkout.user,
                        });
                    }
                    if track.content.has_clap_device() {
                        return Err(RejectReason::ClapTrack { track: id });
                    }
                }
                Scope::TrackStructure(id) => {
                    let checkout = self.doc.track(id).and_then(|t| t.checkout);
                    if let Some(checkout) = checkout.filter(|c| c.user != user) {
                        return Err(RejectReason::CheckedOut {
                            track: id,
                            by: checkout.user,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Gives `txn` the next place in the log and applies it. On refusal nothing changes.
    pub fn sequence(
        &mut self,
        actor: ActorId,
        user: UserId,
        wall_ts: u64,
        txn: Txn,
    ) -> Result<Event, RejectReason> {
        self.check(user, &txn)?;
        let seq = self.head + 1;
        let mut superseded = self.overwritten(user, &txn);
        for op in &txn.ops {
            let changed = apply(&mut self.doc, op).is_some();
            if !changed && let Some(lost) = self.fell_into_removal(user, &txn, op) {
                superseded.push(Superseded {
                    entity: lost.entity(),
                    prop: lost.prop(),
                    lost,
                    lost_actor: actor,
                    lost_user: user,
                    lost_seq: seq,
                });
            }
        }
        let event = Event {
            seq,
            actor,
            user,
            wall_ts,
            body: EventBody::Txn {
                client_txn_id: txn.client_txn_id,
                ops: txn.ops,
                superseded,
            },
        };
        self.head = seq;
        self.tail.push_back(event.clone());
        if self.tail.len() > TAIL {
            self.tail.pop_front();
        }
        Ok(event)
    }

    /// Events the author of `txn` had not seen when making it, newest first.
    fn unseen(&self, txn: &Txn) -> impl Iterator<Item = &Event> {
        self.tail.iter().rev().take_while(|e| e.seq > txn.base_seq)
    }

    /// Writes by other users that `txn` overwrites without having seen them.
    fn overwritten(&self, user: UserId, txn: &Txn) -> Vec<Superseded> {
        let mut found: Vec<Superseded> = Vec::new();
        for op in &txn.ops {
            let (entity, prop) = (op.entity(), op.prop());
            // Adding and removing displace nothing by themselves.
            if prop == PropKey::Existence || found.iter().any(|s| s.entity == entity && s.prop == prop) {
                continue;
            }
            // The newest unseen write to the same property is the value that gets displaced. If
            // it is the author's own, whatever came before was recorded when that was sequenced.
            let last = self.unseen(txn).find_map(|e| {
                Some((
                    e,
                    e.ops()
                        .iter()
                        .rev()
                        .find(|o| o.entity() == entity && o.prop() == prop)?,
                ))
            });
            if let Some((event, lost)) = last
                && event.user != user
                && lost != op
            {
                found.push(Superseded {
                    entity,
                    prop,
                    lost: lost.clone(),
                    lost_actor: event.actor,
                    lost_user: event.user,
                    lost_seq: event.seq,
                });
            }
        }
        found
    }

    /// If `op` did nothing because another user removed its target unseen, the op is what got
    /// lost.
    fn fell_into_removal(&self, user: UserId, txn: &Txn, op: &Op) -> Option<Op> {
        if op.is_removal() {
            return None;
        }
        let entity = op.entity();
        self.unseen(txn)
            .filter(|e| e.user != user)
            .flat_map(|e| e.ops())
            .any(|o| o.is_removal() && entity.is_within(&o.entity()))
            .then(|| op.clone())
    }
}
