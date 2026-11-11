//! Decides what enters the log and in which order. Pure: no I/O, no clock.

use mdaw_model::{ActorId, BlobHash, DeviceKind, Op, Project, PropKey, Scope, TrackId, UserId, apply};
use mdaw_proto::{
    CommitRequest, Event, EventBody, RejectReason, ReleaseReason, Sample, SampleKind, Superseded, Txn,
    apply_event,
};
use std::collections::{BTreeSet, VecDeque};

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
            if op.adds_plugin() {
                return Err(RejectReason::PluginOutsideCommit);
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
                    if track.content.has_plugin() {
                        return Err(RejectReason::PluginTrack { track: id });
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
        // On a copy, which costs nothing: a transaction that would grow the project past its
        // limits leaves it as it was.
        let mut doc = self.doc.clone();
        for op in &txn.ops {
            op.fits(&doc).map_err(|e| RejectReason::Invalid(e.to_string()))?;
            let changed = apply(&mut doc, op).is_some();
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
        self.doc = doc;
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
        Ok(self.push(event))
    }

    /// Takes `track` out of live editing for `user`.
    pub fn checkout(
        &mut self,
        actor: ActorId,
        user: UserId,
        wall_ts: u64,
        track: TrackId,
    ) -> Result<Event, RejectReason> {
        let current = self.doc.track(track).ok_or(RejectReason::NoSuchTrack)?;
        if let Some(checkout) = current.checkout {
            return Err(RejectReason::CheckedOut {
                track,
                by: checkout.user,
            });
        }
        Ok(self.append(actor, user, wall_ts, EventBody::Checkout { track }))
    }

    /// Ends the checkout of `track`: `user`'s own, or with `forced` anyone's.
    pub fn release(
        &mut self,
        actor: ActorId,
        user: UserId,
        wall_ts: u64,
        track: TrackId,
        forced: bool,
    ) -> Result<Event, RejectReason> {
        let current = self.doc.track(track).ok_or(RejectReason::NoSuchTrack)?;
        let checkout = current.checkout.ok_or(RejectReason::NotCheckedOut { track })?;
        let reason = if forced {
            ReleaseReason::Forced { owner: checkout.user }
        } else if checkout.user == user {
            ReleaseReason::Abandoned
        } else {
            return Err(RejectReason::NotOwner { track });
        };
        Ok(self.append(actor, user, wall_ts, EventBody::Release { track, reason }))
    }

    /// Hands in the work of `user`'s checkout. `has_blob` tells which blobs the server holds.
    pub fn commit(
        &mut self,
        actor: ActorId,
        user: UserId,
        wall_ts: u64,
        commit: CommitRequest,
        has_blob: impl Fn(&BlobHash) -> bool,
    ) -> Result<Event, RejectReason> {
        let track = commit.track;
        let current = self.doc.track(track).ok_or(RejectReason::NoSuchTrack)?;
        if current.checkout.is_none_or(|c| c.user != user) {
            return Err(RejectReason::NotOwner { track });
        }
        for op in &commit.ops {
            op.validate().map_err(|e| RejectReason::Invalid(e.to_string()))?;
            if op.scope() != Scope::TrackContent(track) {
                return Err(RejectReason::OutOfScope);
            }
        }
        for op in &commit.project_fix {
            op.validate().map_err(|e| RejectReason::Invalid(e.to_string()))?;
            if op.scope() != Scope::Project || op.adds_plugin() {
                return Err(RejectReason::OutOfScope);
            }
        }
        // What the track will be, to see what it needs.
        let mut after = (*current.content).clone();
        {
            let mut doc = self.doc.clone();
            for op in commit.ops.iter().chain(&commit.project_fix) {
                op.fits(&doc).map_err(|e| RejectReason::Invalid(e.to_string()))?;
                apply(&mut doc, op);
            }
            if let Some(t) = doc.track(track) {
                after = (*t.content).clone();
            }
        }
        if after.has_plugin() && commit.stem.is_none() {
            return Err(RejectReason::StemRequired);
        }
        let states = after.devices.iter().filter_map(|d| match &d.kind {
            DeviceKind::Plugin { state, .. } => *state,
            DeviceKind::Builtin { .. } => None,
        });
        let blobs = commit
            .entry
            .samples
            .iter()
            .map(|s| s.blob)
            .chain(commit.stem.map(|s| s.blob))
            .chain(states);
        for blob in blobs {
            if !has_blob(&blob) {
                return Err(RejectReason::MissingBlob(blob));
            }
        }
        let body = EventBody::Commit {
            track,
            ops: commit.ops,
            project_fix: commit.project_fix,
            stem: commit.stem,
            entry: commit.entry,
        };
        Ok(self.append(actor, user, wall_ts, body))
    }

    /// Adds a sample to the entry of event `entry_seq`: how the track sounded with an edit of
    /// `user` that the event displaced. One per track of such edits, from that user only.
    pub fn attach(
        &mut self,
        actor: ActorId,
        user: UserId,
        wall_ts: u64,
        entry_seq: u64,
        sample: Sample,
        has_blob: impl Fn(&BlobHash) -> bool,
    ) -> Result<Event, RejectReason> {
        // Attaching comes right after the event, so it is still in the tail.
        let entry = self.tail.iter().find(|e| e.seq == entry_seq);
        let Some(EventBody::Txn { superseded, .. }) = entry.map(|e| &e.body) else {
            return Err(RejectReason::NoSuchEntry);
        };
        let tracks: BTreeSet<TrackId> = superseded
            .iter()
            .filter(|lost| lost.lost_user == user)
            .filter_map(|lost| lost.entity.track())
            .collect();
        let attached = self
            .tail
            .iter()
            .filter(|e| e.user == user)
            .filter(|e| matches!(e.body, EventBody::Attach { entry_seq: s, .. } if s == entry_seq))
            .count();
        if sample.kind != SampleKind::Before || attached >= tracks.len() {
            return Err(RejectReason::NoSuchEntry);
        }
        if !has_blob(&sample.blob) {
            return Err(RejectReason::MissingBlob(sample.blob));
        }
        Ok(self.append(actor, user, wall_ts, EventBody::Attach { entry_seq, sample }))
    }

    /// The latest events the changelog shows, oldest first.
    pub fn history(&self, limit: usize) -> Vec<Event> {
        let mut notable: Vec<Event> = self
            .tail
            .iter()
            .rev()
            .filter(|e| e.is_notable())
            .take(limit)
            .cloned()
            .collect();
        notable.reverse();
        notable
    }

    fn append(&mut self, actor: ActorId, user: UserId, wall_ts: u64, body: EventBody) -> Event {
        let event = Event {
            seq: self.head + 1,
            actor,
            user,
            wall_ts,
            body,
        };
        apply_event(&mut self.doc, &event);
        self.push(event)
    }

    /// Puts an event whose effect is in `doc` already at the end of the log.
    fn push(&mut self, event: Event) -> Event {
        self.head = event.seq;
        self.tail.push_back(event.clone());
        if self.tail.len() > TAIL {
            self.tail.pop_front();
        }
        event
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
                        .filter(|o| o.entity() == entity && o.prop() == prop)
                        .last()?,
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
