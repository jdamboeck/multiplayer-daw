use mdaw_model::{ActorId, IdGen, Op, Project, PropKey, apply, apply_all};
use mdaw_proto::{Event, SyncData, Txn, TxnId, apply_event};
use std::collections::VecDeque;
use std::fmt;

/// Undo steps kept per client.
const UNDO_LIMIT: usize = 500;

/// Groups the edits of one continuous gesture (a knob drag) into a single undo step. Edits
/// submitted with the same gesture, one after the other, merge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Gesture(pub u64);

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SyncError {
    /// An event arrived out of order. The connection has to be resynchronised.
    Gap { expected: u64, got: u64 },
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::Gap { expected, got } => write!(f, "expected event {expected}, got {got}"),
        }
    }
}

impl std::error::Error for SyncError {}

/// An op this client applied, with the op that takes it back.
#[derive(Clone, Debug)]
struct Step {
    forward: Op,
    inverse: Op,
}

#[derive(Clone, Debug)]
struct UndoEntry {
    /// In the order they were applied.
    steps: Vec<Step>,
    gesture: Option<Gesture>,
}

/// One of this client's transactions the server has not confirmed yet.
#[derive(Clone, Debug)]
struct Pending {
    txn: Txn,
    /// The server said it sequenced this transaction as this event, which has not reached the
    /// replica yet.
    accepted: Option<u64>,
}

/// One client's copy of the project.
///
/// `confirmed` is the document as of the last event received. `pending` are this client's own
/// transactions whose event has not been received. What the user sees and hears is `view`:
/// confirmed with pending applied on top.
///
/// A pending transaction leaves `pending` in exactly one of these ways, whatever order the
/// messages about it arrive in:
///
/// 1. Its event arrives ([`Replica::on_event`], or among the events of
///    [`Replica::resync`]): matched by [`TxnId`], wherever it is in the queue. The event is in
///    `confirmed` now, so the transaction must not stay on top of it.
/// 2. `Accepted { txn, seq }` arrives ([`Replica::on_accepted`]) with `seq` at or before the
///    replica's position: the event is in `confirmed` already (it came inside a snapshot), so the
///    entry goes. That is the only way an entry can be in `confirmed` without its event having
///    been fed one by one.
/// 3. `Accepted` arrives with a `seq` the replica has not reached: the event is still on its way.
///    The entry stays, marked, and goes with case 1 when the event comes; or, should a snapshot
///    at or past `seq` come first, with the [`Replica::resync`] that takes it in.
/// 4. `Rejected` arrives ([`Replica::on_rejected`]): the entry goes, and the view falls back.
///
/// `Accepted` after the event (the server answers a transaction sent twice), `Accepted` or
/// `Rejected` for a transaction no longer pending, and an event of this client's after
/// `Accepted` find nothing to do. Between a snapshot and the `Accepted` that follows it, a
/// transaction already in the snapshot is applied once more on top for the view; ops are total
/// and an add of something that exists does nothing, so the view stays a valid document, and it
/// returns to the server's as soon as the `Accepted` arrives.
pub struct Replica {
    ids: IdGen,
    confirmed: Project,
    seq: u64,
    pending: VecDeque<Pending>,
    view: Project,
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
}

impl Replica {
    /// A replica holding `project`, which is the document after event `seq`.
    pub fn new(actor: ActorId, project: Project, seq: u64) -> Self {
        Self {
            ids: IdGen::new(actor),
            view: project.clone(),
            confirmed: project,
            seq,
            pending: VecDeque::new(),
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    pub fn actor(&self) -> ActorId {
        self.ids.actor()
    }

    /// Source of IDs for new entities.
    pub fn ids(&mut self) -> &mut IdGen {
        &mut self.ids
    }

    /// The document to show and play.
    pub fn view(&self) -> &Project {
        &self.view
    }

    /// The document as the server has it, as far as this client knows.
    pub fn confirmed(&self) -> &Project {
        &self.confirmed
    }

    /// Sequence number of the last event applied.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Own transactions not yet confirmed, oldest first.
    pub fn pending(&self) -> impl ExactSizeIterator<Item = &Txn> {
        self.pending.iter().map(|p| &p.txn)
    }

    /// Takes in transactions made before, by an earlier run of this client, that the server may
    /// not have: they go on top of the view and are pending like any made now. They are not
    /// steps of undo.
    pub fn restore_pending(&mut self, txns: impl IntoIterator<Item = Txn>) {
        let before = self.pending.len();
        for txn in txns {
            if !self.pending.iter().any(|p| p.txn.id == txn.id) {
                self.pending.push_back(Pending { txn, accepted: None });
            }
        }
        if self.pending.len() != before {
            self.rebuild();
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Applies an edit locally and returns the transaction to send. Ops that change nothing are
    /// dropped; `None` if none is left.
    pub fn submit(&mut self, ops: Vec<Op>) -> Option<Txn> {
        self.submit_as(ops, None)
    }

    /// As [`Replica::submit`], merging into the previous undo step if that came from the same
    /// gesture.
    pub fn submit_gesture(&mut self, ops: Vec<Op>, gesture: Gesture) -> Option<Txn> {
        self.submit_as(ops, Some(gesture))
    }

    fn submit_as(&mut self, ops: Vec<Op>, gesture: Option<Gesture>) -> Option<Txn> {
        let mut steps = Vec::with_capacity(ops.len());
        for op in ops {
            if op.validate().is_err() {
                debug_assert!(false, "invalid op submitted: {op:?}");
                continue;
            }
            if let Some(inverse) = apply(&mut self.view, &op) {
                steps.push(Step { forward: op, inverse });
            }
        }
        if steps.is_empty() {
            return None;
        }
        let txn = self.enqueue(steps.iter().map(|s| s.forward.clone()).collect());
        self.redo.clear();
        match self.undo.last_mut() {
            Some(top) if gesture.is_some() && top.gesture == gesture => merge(&mut top.steps, steps),
            _ => {
                self.undo.push(UndoEntry { steps, gesture });
                if self.undo.len() > UNDO_LIMIT {
                    self.undo.remove(0);
                }
            }
        }
        Some(txn)
    }

    /// Wraps ops already applied to the view into a pending transaction, under an identity of
    /// its own that it keeps for good.
    fn enqueue(&mut self, ops: Vec<Op>) -> Txn {
        let txn = Txn {
            id: TxnId::random(),
            base_seq: self.seq,
            ops,
        };
        self.pending.push_back(Pending {
            txn: txn.clone(),
            accepted: None,
        });
        txn
    }

    /// Takes back this client's most recent edit that can still be taken back. Parts of it that
    /// someone else has overwritten since are left alone.
    ///
    /// Returns the transaction to send, if anything could be taken back.
    pub fn undo(&mut self) -> Option<Txn> {
        while let Some(entry) = self.undo.pop() {
            if let Some((ops, reverse)) = self.revert(entry) {
                self.redo.push(reverse);
                return Some(self.enqueue(ops));
            }
        }
        None
    }

    /// Reapplies the edit most recently undone.
    pub fn redo(&mut self) -> Option<Txn> {
        while let Some(entry) = self.redo.pop() {
            if let Some((ops, reverse)) = self.revert(entry) {
                self.undo.push(reverse);
                return Some(self.enqueue(ops));
            }
        }
        None
    }

    /// Applies the inverses of `entry`, newest first. Returns the ops applied and the entry
    /// that reverts them in turn.
    fn revert(&mut self, entry: UndoEntry) -> Option<(Vec<Op>, UndoEntry)> {
        let mut ops = Vec::new();
        let mut steps = Vec::new();
        for step in entry.steps.into_iter().rev() {
            // Only take back what is still as this client left it: if applying the op again
            // would change something, someone has written there since.
            let mut probe = self.view.clone();
            if apply(&mut probe, &step.forward).is_some() {
                continue;
            }
            if let Some(inverse) = apply(&mut self.view, &step.inverse) {
                ops.push(step.inverse.clone());
                steps.push(Step {
                    forward: step.inverse,
                    inverse,
                });
            }
        }
        if ops.is_empty() {
            return None;
        }
        Some((ops, UndoEntry { steps, gesture: None }))
    }

    /// Feeds the next event of the log.
    pub fn on_event(&mut self, event: &Event) -> Result<(), SyncError> {
        if event.seq != self.seq + 1 {
            return Err(SyncError::Gap {
                expected: self.seq + 1,
                got: event.seq,
            });
        }
        self.seq = event.seq;
        apply_event(&mut self.confirmed, event);

        match self.pending.iter().position(|p| p.txn.id == event.txn) {
            // The oldest pending transaction: the view already contains this edit and everything
            // pending after it, in this order.
            Some(0) => {
                self.pending.pop_front();
                if self.pending.is_empty() {
                    debug_assert!(self.confirmed == self.view, "view diverged from the server");
                    // Equal in content; share the view's allocations so the next foreign event
                    // does not make every track look changed.
                    self.confirmed = self.view.clone();
                }
            }
            // Older ones still wait for their answer (they were sent again after a reconnect,
            // or wait for an upload): the view is built anew without this one.
            Some(at) => {
                self.pending.remove(at);
                self.rebuild();
            }
            None => self.rebuild(),
        }
        Ok(())
    }

    /// The server says it sequenced `txn` as event `seq` before: the answer to a transaction
    /// sent again.
    pub fn on_accepted(&mut self, txn: TxnId, seq: u64) {
        let Some(at) = self.pending.iter().position(|p| p.txn.id == txn) else {
            // Confirmed by its event already.
            return;
        };
        if seq <= self.seq {
            // The event came inside a snapshot: its effect is in `confirmed` already.
            self.pending.remove(at);
            self.rebuild();
        } else {
            // The event is on its way; it confirms the transaction when it arrives.
            self.pending[at].accepted = Some(seq);
        }
    }

    /// Continues on a new connection, after the server's `Welcome`.
    ///
    /// `sync` brings the confirmed document to `head_seq`. Returns every transaction still
    /// pending, oldest first, to be sent again with their identities: the server answers those it
    /// has seen before with their original outcome.
    ///
    /// On an error nothing has changed, and the way out is to join afresh.
    pub fn resync(&mut self, actor: ActorId, head_seq: u64, sync: SyncData) -> Result<Vec<Txn>, SyncError> {
        match sync {
            SyncData::Snapshot(project) => {
                self.confirmed = project;
                // Accepted before, and in the snapshot now.
                self.pending
                    .retain(|p| p.accepted.is_none_or(|seq| seq > head_seq));
            }
            SyncData::Events(events) => {
                // Checked before anything changes, so a refused resync leaves the replica whole.
                let mut next = self.seq + 1;
                for event in &events {
                    if event.seq != next {
                        return Err(SyncError::Gap {
                            expected: next,
                            got: event.seq,
                        });
                    }
                    next += 1;
                }
                if next != head_seq + 1 {
                    return Err(SyncError::Gap {
                        expected: next,
                        got: head_seq + 1,
                    });
                }
                for event in &events {
                    apply_event(&mut self.confirmed, event);
                    self.pending.retain(|p| p.txn.id != event.txn);
                }
            }
        }
        self.seq = head_seq;
        // IDs minted from here on must not collide with the old connection's.
        if actor != self.ids.actor() {
            self.ids = IdGen::new(actor);
        }
        self.rebuild();
        Ok(self.pending().cloned().collect())
    }

    /// The server refused a transaction, or it cannot be sent: forget it and everything it did
    /// locally. False if it was not pending.
    pub fn on_rejected(&mut self, txn: TxnId) -> bool {
        let before = self.pending.len();
        self.pending.retain(|p| p.txn.id != txn);
        let found = self.pending.len() != before;
        if found {
            self.rebuild();
        }
        found
    }

    fn rebuild(&mut self) {
        let mut view = self.confirmed.clone();
        for pending in &self.pending {
            apply_all(&mut view, &pending.txn.ops);
        }
        self.view = view;
    }
}

/// Folds the steps of a continuing gesture into the steps recorded so far: a property written
/// again keeps its original inverse and takes the new value.
fn merge(into: &mut Vec<Step>, steps: Vec<Step>) {
    for step in steps {
        // Adding and removing are not "the same value written again".
        let settable = step.forward.prop() != PropKey::Existence;
        let same = into.iter_mut().find(|s| {
            settable && s.forward.entity() == step.forward.entity() && s.forward.prop() == step.forward.prop()
        });
        match same {
            Some(existing) => existing.forward = step.forward,
            None => into.push(step),
        }
    }
}
