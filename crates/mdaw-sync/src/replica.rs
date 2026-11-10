use mdaw_model::{ActorId, IdGen, Op, Project, PropKey, apply, apply_all};
use mdaw_proto::{Event, Txn};
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

/// One client's copy of the project.
///
/// `confirmed` is the document as of the last event received. `pending` are this client's own
/// transactions the server has not sequenced yet. What the user sees and hears is `view`:
/// confirmed with pending applied on top.
pub struct Replica {
    ids: IdGen,
    confirmed: Project,
    seq: u64,
    pending: VecDeque<Txn>,
    view: Project,
    next_txn: u64,
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
            next_txn: 1,
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

    /// Own transactions not yet sequenced, oldest first.
    pub fn pending(&self) -> impl ExactSizeIterator<Item = &Txn> {
        self.pending.iter()
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

    /// Wraps ops already applied to the view into a pending transaction.
    fn enqueue(&mut self, ops: Vec<Op>) -> Txn {
        let txn = Txn {
            client_txn_id: self.next_txn,
            base_seq: self.seq,
            ops,
        };
        self.next_txn += 1;
        self.pending.push_back(txn.clone());
        txn
    }

    /// Takes back this client's most recent edit that can still be taken back. Parts of it that
    /// someone else has overwritten since are left alone.
    pub fn undo(&mut self) -> Option<Txn> {
        while let Some(entry) = self.undo.pop() {
            if let Some((txn, reverse)) = self.revert(entry) {
                self.redo.push(reverse);
                return Some(txn);
            }
        }
        None
    }

    /// Reapplies the edit most recently undone.
    pub fn redo(&mut self) -> Option<Txn> {
        while let Some(entry) = self.redo.pop() {
            if let Some((txn, reverse)) = self.revert(entry) {
                self.undo.push(reverse);
                return Some(txn);
            }
        }
        None
    }

    /// Applies the inverses of `entry`, newest first. Returns the transaction and the entry
    /// that reverts it in turn.
    fn revert(&mut self, entry: UndoEntry) -> Option<(Txn, UndoEntry)> {
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
        Some((self.enqueue(ops), UndoEntry { steps, gesture: None }))
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
        apply_all(&mut self.confirmed, event.ops());

        // The server sequences one client's transactions in the order they were sent.
        let own = event.actor == self.actor()
            && self
                .pending
                .front()
                .is_some_and(|txn| event.client_txn_id() == Some(txn.client_txn_id));
        if own {
            // The view already contains this edit and everything pending after it.
            self.pending.pop_front();
            if self.pending.is_empty() {
                debug_assert!(self.confirmed == self.view, "view diverged from the server");
                // Equal in content; share the view's allocations so the next foreign event
                // does not make every track look changed.
                self.confirmed = self.view.clone();
            }
        } else {
            self.rebuild();
        }
        Ok(())
    }

    /// The server refused a transaction: forget it and everything it did locally.
    pub fn on_rejected(&mut self, client_txn_id: u64) {
        let before = self.pending.len();
        self.pending.retain(|txn| txn.client_txn_id != client_txn_id);
        if self.pending.len() != before {
            self.rebuild();
        }
    }

    fn rebuild(&mut self) {
        let mut view = self.confirmed.clone();
        for txn in &self.pending {
            apply_all(&mut view, &txn.ops);
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
