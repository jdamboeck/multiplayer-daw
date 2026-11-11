//! What travels between client and server. The document itself lives in `mdaw-model`; this crate
//! wraps its ops into transactions and into the events of the project's log.

pub mod midi;
mod transport;
mod wire;

use mdaw_model::{
    ActorId, BlobHash, Checkout, EntityRef, Op, Project, PropKey, Stem, TickRange, TrackId, UserId, apply,
};
use serde::{Deserialize, Serialize};

pub use midi::{LiveAction, LiveEvent, MidiIn, MidiOut, MidiPacket};
pub use transport::{Datagram, MAX_LAG_MICROS, MAX_LEAD_MICROS, START_LEAD_MICROS, TransportState};
pub use wire::{
    ALPN, BlobAnswer, BlobRequest, ClientMsg, CommitRequest, FrameError, Framed, Hello, MAX_BLOB, MAX_FRAME,
    MAX_SERVER_FRAME, Peer, Presence, RefuseReason, RejectReason, Request, Resume, ServerMsg, SyncData,
    Welcome, decode, encode, payload_len,
};

/// Bumped on every incompatible change to the wire format or to the meaning of an op.
pub const PROTO_VERSION: u32 = 5;

/// The hash a blob of these bytes is stored and named under.
pub fn blob_hash(bytes: &[u8]) -> BlobHash {
    BlobHash(*blake3::hash(bytes).as_bytes())
}

/// How often the owner of a checkout renews its lease.
pub const LEASE_RENEW_SECS: u64 = 10;
/// A checkout whose lease was not renewed for this long is shown as stale.
pub const LEASE_STALE_SECS: u64 = 60;

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
    /// The event's user takes the track's contents out of live editing. From here on only
    /// their commit changes them.
    Checkout { track: TrackId },
    /// The owner of a checkout hands its work in. The track is live again.
    Commit {
        track: TrackId,
        /// Content ops of this track only: everything the owner did while it was checked out.
        ops: Vec<Op>,
        /// Project-scope ops applied with the commit, after `ops`. Used to put back project
        /// state the work was made against, such as the tempo.
        project_fix: Vec<Op>,
        /// The track as audio, for clients that cannot render it. Replaces the track's stem.
        stem: Option<Stem>,
        entry: ChangelogEntry,
    },
    /// A checkout ends without a commit. The track is live again with the contents it had.
    Release { track: TrackId, reason: ReleaseReason },
    /// A sample that was rendered after the entry it belongs to was sequenced.
    Attach { entry_seq: u64, sample: Sample },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ReleaseReason {
    /// The owner gave the checkout up; the event's user is the owner.
    Abandoned,
    /// The event's user ended the checkout of `owner`.
    Forced { owner: UserId },
}

/// What a commit says about itself in the changelog.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ChangelogEntry {
    /// Written by the committer.
    pub summary: String,
    /// Where the track changed.
    pub ranges: Vec<TickRange>,
    pub samples: Vec<Sample>,
}

/// A short piece of audio that lets others hear a change without opening the project there.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Sample {
    pub kind: SampleKind,
    /// FLAC, 16 bit, 48 kHz, stereo.
    pub blob: BlobHash,
    /// The part of the timeline it plays.
    pub range: TickRange,
    /// Its length in milliseconds.
    pub ms: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum SampleKind {
    /// The changed track alone, after the change.
    Solo,
    /// All tracks, after the change.
    InContext,
    /// The track alone as it was before an edit was displaced.
    Before,
    /// All tracks, with the tempo the checkout started with.
    ProjectBase,
    /// All tracks, with the tempo at the time of the commit.
    ProjectCurrent,
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
            _ => None,
        }
    }

    /// The document ops this event carries, in application order.
    pub fn ops(&self) -> impl Iterator<Item = &Op> {
        let (first, second): (&[Op], &[Op]) = match &self.body {
            EventBody::Txn { ops, .. } => (ops, &[]),
            EventBody::Commit { ops, project_fix, .. } => (ops, project_fix),
            EventBody::Checkout { .. } | EventBody::Release { .. } | EventBody::Attach { .. } => (&[], &[]),
        };
        first.iter().chain(second)
    }

    /// True for events the changelog shows: everything but a live edit that displaced nothing.
    pub fn is_notable(&self) -> bool {
        match &self.body {
            EventBody::Txn { superseded, .. } => !superseded.is_empty(),
            _ => true,
        }
    }
}

/// Folds one event into the document. Server and clients build their state with nothing else,
/// so they agree on it.
pub fn apply_event(project: &mut Project, event: &Event) {
    for op in event.ops() {
        apply(project, op);
    }
    match &event.body {
        EventBody::Txn { .. } | EventBody::Attach { .. } => {}
        EventBody::Checkout { track } => {
            let checkout = Checkout {
                user: event.user,
                base_seq: event.seq,
            };
            project.set_checkout(*track, Some(checkout));
        }
        EventBody::Commit { track, stem, .. } => {
            project.set_checkout(*track, None);
            project.set_stem(*track, *stem);
        }
        EventBody::Release { track, .. } => {
            project.set_checkout(*track, None);
        }
    }
}
