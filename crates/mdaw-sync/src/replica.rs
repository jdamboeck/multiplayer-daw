use imbl::OrdMap;
use mdaw_model::{
    ActorId, IdGen, Op, Project, PropKey, Scope, TempoId, TempoPoint, TrackId, UserId, apply, apply_all,
};
use mdaw_proto::{Event, EventBody, ReleaseReason, SyncData, Txn, apply_event};
use std::collections::{BTreeMap, VecDeque};
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

/// A track this client's user has checked out. Edits to its contents stay here, in the
/// journal, until they are committed; the view shows the track with the journal applied.
#[derive(Clone, PartialEq, Debug)]
pub struct Workspace {
    pub track: TrackId,
    /// The edits so far, in order. They apply to the track's confirmed contents, which nobody
    /// else can change while the checkout lasts.
    pub journal: Vec<Op>,
    /// The tempo points when the checkout began: what the work was made against.
    pub base_tempo: OrdMap<TempoId, TempoPoint>,
    /// A commit of this workspace is on its way. No more edits are taken.
    pub committing: bool,
}

/// How a checkout of this client's user came to an end.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ended {
    /// The journal is in the document now.
    Committed,
    /// Given up by its owner: the journal is thrown away.
    Abandoned,
    /// Someone ended it. The journal is all that is left of the work.
    Forced { by: UserId },
    /// The track is gone, or the checkout ended in events this client did not see one by one.
    Lost,
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
    /// Whose checkouts are this client's.
    user: Option<UserId>,
    workspaces: BTreeMap<TrackId, Workspace>,
    ended: Vec<(Workspace, Ended)>,
    /// Counts the local edits applied to the view.
    revision: u64,
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
            user: None,
            workspaces: BTreeMap::new(),
            ended: Vec::new(),
            revision: 0,
        }
    }

    /// Tells the replica which user it belongs to, so it knows its checkouts. Until then it
    /// has none.
    pub fn set_user(&mut self, user: UserId) {
        if self.user != Some(user) {
            self.user = Some(user);
            self.reconcile();
            self.rebuild();
        }
    }

    pub fn user(&self) -> Option<UserId> {
        self.user
    }

    /// Goes up with every local edit, undo and redo that changed the view, also those that
    /// only went into a journal and so return no transaction.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// This client's checkouts.
    pub fn workspaces(&self) -> impl Iterator<Item = &Workspace> {
        self.workspaces.values()
    }

    pub fn workspace(&self, track: TrackId) -> Option<&Workspace> {
        self.workspaces.get(&track)
    }

    /// Replaces the journal of a checkout, as when it is read back from disk after a restart.
    /// False if `track` is not checked out by this client.
    pub fn restore_journal(
        &mut self,
        track: TrackId,
        journal: Vec<Op>,
        base_tempo: OrdMap<TempoId, TempoPoint>,
    ) -> bool {
        let Some(workspace) = self.workspaces.get_mut(&track) else {
            return false;
        };
        workspace.journal = journal;
        workspace.base_tempo = base_tempo;
        self.rebuild();
        true
    }

    /// Freezes the workspace of `track` for a commit and returns it. `None` if there is no
    /// such workspace or it is being committed already.
    pub fn begin_commit(&mut self, track: TrackId) -> Option<Workspace> {
        let workspace = self.workspaces.get_mut(&track).filter(|w| !w.committing)?;
        workspace.committing = true;
        Some(workspace.clone())
    }

    /// The commit did not go through: the workspace takes edits again.
    pub fn cancel_commit(&mut self, track: TrackId) {
        if let Some(workspace) = self.workspaces.get_mut(&track) {
            workspace.committing = false;
        }
    }

    /// Checkouts of this client's user that ended since the last call, with their journals.
    pub fn take_ended(&mut self) -> Vec<(Workspace, Ended)> {
        std::mem::take(&mut self.ended)
    }

    /// True when edits to the contents of `track` are taken: it is live, or checked out here and
    /// not being committed.
    pub fn can_edit_content(&self, track: TrackId) -> bool {
        match self.confirmed.track(track).and_then(|t| t.checkout) {
            None => true,
            Some(_) => self.workspaces.get(&track).is_some_and(|w| !w.committing),
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
            if self.frozen(&op) {
                continue;
            }
            if let Some(inverse) = apply(&mut self.view, &op) {
                steps.push(Step { forward: op, inverse });
            }
        }
        if steps.is_empty() {
            return None;
        }
        self.revision += 1;
        let txn = self.dispatch(steps.iter().map(|s| s.forward.clone()).collect());
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
        txn
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

    /// True for an op on a workspace that is being committed.
    fn frozen(&self, op: &Op) -> bool {
        match op.scope() {
            Scope::TrackContent(track) => self.workspaces.get(&track).is_some_and(|w| w.committing),
            _ => false,
        }
    }

    /// Ops already applied to the view: those on a checked-out track go to its journal, the
    /// rest into a pending transaction, if any are left.
    fn dispatch(&mut self, ops: Vec<Op>) -> Option<Txn> {
        let mut live = Vec::with_capacity(ops.len());
        for op in ops {
            match op.scope() {
                Scope::TrackContent(track) if self.workspaces.contains_key(&track) => {
                    self.workspaces.get_mut(&track).expect("checked").journal.push(op);
                }
                _ => live.push(op),
            }
        }
        (!live.is_empty()).then(|| self.enqueue(live))
    }

    /// Takes back this client's most recent edit that can still be taken back. Parts of it that
    /// someone else has overwritten since are left alone.
    ///
    /// Returns the transaction to send, if the undone edit was not wholly inside a checkout.
    pub fn undo(&mut self) -> Option<Txn> {
        while let Some(entry) = self.undo.pop() {
            if let Some((ops, reverse)) = self.revert(entry) {
                self.redo.push(reverse);
                return self.dispatch(ops);
            }
        }
        None
    }

    /// Reapplies the edit most recently undone.
    pub fn redo(&mut self) -> Option<Txn> {
        while let Some(entry) = self.redo.pop() {
            if let Some((ops, reverse)) = self.revert(entry) {
                self.undo.push(reverse);
                return self.dispatch(ops);
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
            if self.frozen(&step.inverse) {
                continue;
            }
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
        self.revision += 1;
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
        let workspaces_changed = self.follow_checkouts(event);

        // The server sequences one client's transactions in the order they were sent.
        let own = event.actor == self.actor()
            && self
                .pending
                .front()
                .is_some_and(|txn| event.client_txn_id() == Some(txn.client_txn_id));
        if own && !workspaces_changed {
            // The view already contains this edit and everything pending after it.
            self.pending.pop_front();
            if self.pending.is_empty() && self.workspaces.is_empty() {
                debug_assert!(self.confirmed == self.view, "view diverged from the server");
                // Equal in content; share the view's allocations so the next foreign event
                // does not make every track look changed.
                self.confirmed = self.view.clone();
            }
        } else {
            if own {
                self.pending.pop_front();
            }
            self.rebuild();
        }
        Ok(())
    }

    /// Starts and ends workspaces as the event says. True if any did.
    fn follow_checkouts(&mut self, event: &Event) -> bool {
        let Some(user) = self.user else {
            return false;
        };
        let changed = match &event.body {
            EventBody::Checkout { track } if event.user == user => {
                let workspace = Workspace {
                    track: *track,
                    journal: Vec::new(),
                    base_tempo: self.confirmed.tempo.clone(),
                    committing: false,
                };
                self.workspaces.insert(*track, workspace);
                true
            }
            EventBody::Commit { track, .. } => self.end(*track, Ended::Committed),
            EventBody::Release { track, reason } => {
                let how = match reason {
                    ReleaseReason::Abandoned => Ended::Abandoned,
                    ReleaseReason::Forced { .. } if event.user == user => Ended::Abandoned,
                    ReleaseReason::Forced { .. } => Ended::Forced { by: event.user },
                };
                self.end(*track, how)
            }
            _ => false,
        };
        // A removed track takes its checkout with it.
        self.reconcile() || changed
    }

    fn end(&mut self, track: TrackId, how: Ended) -> bool {
        match self.workspaces.remove(&track) {
            Some(workspace) => {
                self.ended.push((workspace, how));
                true
            }
            None => false,
        }
    }

    /// Makes the workspaces match the checkouts of the confirmed document: a checkout of this
    /// user without a workspace gets an empty one, a workspace without a checkout ends. True if
    /// anything changed.
    fn reconcile(&mut self) -> bool {
        let Some(user) = self.user else {
            return false;
        };
        let confirmed = &self.confirmed;
        let gone: Vec<TrackId> = self
            .workspaces
            .keys()
            .filter(|id| {
                confirmed
                    .track(**id)
                    .and_then(|t| t.checkout)
                    .is_none_or(|c| c.user != user)
            })
            .copied()
            .collect();
        let new: Vec<TrackId> = confirmed
            .tracks
            .iter()
            .filter(|(id, t)| t.checkout.is_some_and(|c| c.user == user) && !self.workspaces.contains_key(id))
            .map(|(id, _)| *id)
            .collect();
        let changed = !gone.is_empty() || !new.is_empty();
        for track in gone {
            self.end(track, Ended::Lost);
        }
        for track in new {
            let workspace = Workspace {
                track,
                journal: Vec::new(),
                base_tempo: self.confirmed.tempo.clone(),
                committing: false,
            };
            self.workspaces.insert(track, workspace);
        }
        changed
    }

    /// Continues on a new connection, after the server's `Welcome`.
    ///
    /// `sync` brings the confirmed document to `head_seq`. `acked` is the last of this replica's
    /// transactions the server dealt with on the previous connection; those leave the pending
    /// queue. Returns what is still pending, to be sent again under the new `actor`.
    ///
    /// On an error nothing has changed, and the way out is to join afresh.
    pub fn resync(
        &mut self,
        actor: ActorId,
        head_seq: u64,
        sync: SyncData,
        acked: Option<u64>,
    ) -> Result<Vec<Txn>, SyncError> {
        match sync {
            SyncData::Snapshot(project) => {
                self.confirmed = project;
                self.reconcile();
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
                    self.follow_checkouts(event);
                }
            }
        }
        self.seq = head_seq;
        if let Some(acked) = acked {
            self.pending.retain(|txn| txn.client_txn_id > acked);
        }
        // IDs minted from here on must not collide with the old connection's.
        if actor != self.ids.actor() {
            self.ids = IdGen::new(actor);
        }
        self.rebuild();
        Ok(self.pending.iter().cloned().collect())
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
        for workspace in self.workspaces.values() {
            let Some(base) = self.confirmed.track(workspace.track) else {
                continue;
            };
            // Pending live ops cannot touch these contents: the server would refuse them.
            view.set_content(workspace.track, base.content.clone());
            apply_all(&mut view, &workspace.journal);
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
