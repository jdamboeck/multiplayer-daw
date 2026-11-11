//! Messages of the control stream and how they are framed.
//!
//! A frame is a `u32` little-endian length followed by that many bytes of postcard. The first
//! bidirectional stream the client opens is the control stream, on which it speaks first with
//! [`Hello`]. Every further stream it opens carries one blob: a [`BlobRequest`] frame, answered
//! with [`BlobAnswer`] frames and raw bytes.

use crate::{ChangelogEntry, Event, Sample, TransportState, Txn};
use mdaw_model::{ActorId, BlobHash, EntityRef, Op, Project, Stem, Tick, TrackId, UserId};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt;

/// ALPN protocol name of the QUIC connection.
pub const ALPN: &[u8] = b"mdaw/0";

/// Largest frame a client may send, and the server accepts. Every event derives from one such
/// message, so this bounds the size of an event too.
pub const MAX_FRAME: usize = 16 << 20;

/// Largest frame the server sends, and a client accepts: a snapshot of the whole project, or a
/// catch-up made of many events. The server is the one a client chose to trust.
pub const MAX_SERVER_FRAME: usize = 256 << 20;

/// A message that travels as one frame, with the largest size accepted for it.
pub trait Framed: Serialize + DeserializeOwned {
    const MAX: usize = MAX_FRAME;
}

impl Framed for ClientMsg {}
impl Framed for BlobRequest {}
impl Framed for BlobAnswer {}
impl Framed for ServerMsg {
    const MAX: usize = MAX_SERVER_FRAME;
}

/// Largest blob either side accepts.
pub const MAX_BLOB: u64 = 256 << 20;

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum ClientMsg {
    /// First message on the stream.
    Hello(Hello),
    Submit(Txn),
    /// Relayed to the other clients, never stored. At most 10 per second.
    Presence(Presence),
    /// Asks for a new state of the shared transport. Any client may, and each request is
    /// answered with exactly one [`ServerMsg::Transport`] that names the asker.
    SetTransport(TransportState),
    /// Takes the track's contents out of live editing, for the sender alone to work on.
    Checkout {
        track: TrackId,
    },
    /// "I am still working on it." Sent every [`crate::LEASE_RENEW_SECS`] by the owner of a
    /// checkout; without it the checkout is shown as stale to everyone.
    LeaseRenew {
        track: TrackId,
    },
    /// Gives the sender's checkout up. The track keeps the contents it had.
    Release {
        track: TrackId,
    },
    /// Ends someone else's checkout. Anyone may.
    ForceRelease {
        track: TrackId,
    },
    /// Hands in the work of a checkout. Every blob it names must have been uploaded before.
    Commit(Box<CommitRequest>),
    /// Adds a sample to an entry of the log that exists already.
    Attach {
        entry_seq: u64,
        sample: Sample,
    },
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct CommitRequest {
    pub track: TrackId,
    pub ops: Vec<Op>,
    pub project_fix: Vec<Op>,
    pub stem: Option<Stem>,
    pub entry: ChangelogEntry,
}

/// What a client asked for outside of a transaction. Names the request in a refusal.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Request {
    Checkout(TrackId),
    Release(TrackId),
    ForceRelease(TrackId),
    Commit(TrackId),
    Attach(u64),
}

/// The first frame of a blob stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum BlobRequest {
    /// The client offers a blob of `len` bytes. After [`BlobAnswer::Send`] it writes the bytes
    /// and finishes the stream, and the server answers once more.
    Put { hash: BlobHash, len: u64 },
    /// The client asks for a blob. After [`BlobAnswer::Found`] the server writes the bytes.
    Get { hash: BlobHash },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum BlobAnswer {
    /// To a `Put`: go ahead.
    Send,
    /// To a `Put`: the server has the blob, either from before or as of now.
    Stored,
    /// To a `Put`: too large, or the bytes did not hash to what was announced.
    Refused,
    /// To a `Get`: `len` bytes follow.
    Found { len: u64 },
    /// To a `Get`: the server has no such blob.
    Missing,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Hello {
    pub proto_version: u32,
    /// The session secret from the join string.
    pub token: String,
    /// Random key of the client install. The server maps it to a [`UserId`].
    pub user_key: [u8; 16],
    /// Display name.
    pub name: String,
    /// Set when reconnecting, to receive only what was missed.
    pub resume: Option<Resume>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Resume {
    /// The actor of the connection being replaced.
    pub prev_actor: ActorId,
    /// Proof that the previous connection was this client's.
    pub session_secret: u64,
    /// The last event the client received.
    pub last_seq: u64,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum ServerMsg {
    /// Answer to an accepted [`Hello`].
    Welcome(Box<Welcome>),
    /// Answer to a [`Hello`] the server turns down. The server closes the connection after it.
    Refused(RefuseReason),
    /// The next entry of the log. The only way the document changes.
    Event(Event),
    /// A submitted transaction was not sequenced. Nothing of it was applied.
    Rejected {
        client_txn_id: u64,
        reason: RejectReason,
    },
    /// Everyone connected, including the receiver. Sent whenever the set changes.
    Peers(Vec<Peer>),
    /// Another client's presence.
    Presence { actor: ActorId, presence: Presence },
    /// The state of the shared transport. Sent to everyone when it changes, the client that
    /// asked included.
    Transport {
        state: TransportState,
        /// The client whose request this answers; `None` when the server moved the transport
        /// itself. A client that counts its unanswered requests knows from this when its own
        /// idea of the transport is the older one.
        by: Option<ActorId>,
    },
    /// A request other than a transaction was turned down. Nothing changed.
    Denied { request: Request, reason: RejectReason },
    /// The checkouts whose owners have not renewed their lease in time. Sent whenever the set
    /// changes.
    Stale(Vec<TrackId>),
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Welcome {
    /// Fresh for this connection; new entity IDs are minted under it.
    pub actor: ActorId,
    pub user: UserId,
    /// Present it in [`Resume`] to replace this connection later.
    pub session_secret: u64,
    /// The log position the client is at after applying `sync`.
    pub head_seq: u64,
    pub sync: SyncData,
    /// When resuming: the highest `client_txn_id` of the previous actor the server has dealt
    /// with. Pending transactions up to it must not be sent again.
    pub acked: Option<u64>,
    pub peers: Vec<Peer>,
    /// The shared transport as it is now.
    pub transport: TransportState,
    /// With a snapshot: the latest events the changelog shows, oldest first. Their effect on
    /// the document is in the snapshot already.
    pub history: Vec<Event>,
    /// The checkouts without a fresh lease.
    pub stale: Vec<TrackId>,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum SyncData {
    /// The whole document at `head_seq`.
    Snapshot(Project),
    /// The events after `Resume::last_seq`, up to `head_seq`.
    Events(Vec<Event>),
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Peer {
    pub actor: ActorId,
    pub user: UserId,
    pub name: String,
}

/// What a client is doing right now. Advisory: it dims and blocks on other clients' screens,
/// the server enforces nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Presence {
    /// The entity being dragged or edited, which others should leave alone.
    pub editing: Option<EntityRef>,
    pub playhead: Option<Tick>,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum RefuseReason {
    BadToken,
    /// The client speaks another protocol version than the server's, which is given.
    ProtoVersion(u32),
    /// The first message was not a `Hello`.
    Protocol,
}

impl fmt::Display for RefuseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RefuseReason::BadToken => write!(f, "the server did not accept the session token"),
            RefuseReason::ProtoVersion(server) => write!(
                f,
                "the server speaks protocol version {server}, this client {}",
                crate::PROTO_VERSION
            ),
            RefuseReason::Protocol => write!(f, "the server did not understand this client"),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum RejectReason {
    /// An op failed its range checks, or the transaction was empty.
    Invalid(String),
    /// `base_seq` is ahead of the log.
    FutureBase,
    /// The track's contents are locked by a checkout.
    CheckedOut {
        track: TrackId,
        by: UserId,
    },
    /// Tracks with plugins are edited only through checkout.
    PluginTrack {
        track: TrackId,
    },
    /// Plugins are added only by a commit.
    PluginOutsideCommit,
    NoSuchTrack,
    /// The request needs the sender to own the track's checkout.
    NotOwner {
        track: TrackId,
    },
    /// There is no checkout to release.
    NotCheckedOut {
        track: TrackId,
    },
    /// A commit may only carry content ops of its track and project-scope fixes.
    OutOfScope,
    /// A blob the request names was not uploaded.
    MissingBlob(BlobHash),
    /// A track with plugins must be committed with a stem.
    StemRequired,
    /// The entry to attach to is not in the log.
    NoSuchEntry,
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RejectReason::Invalid(why) => write!(f, "invalid edit: {why}"),
            RejectReason::FutureBase => write!(f, "the edit refers to events the server does not have"),
            RejectReason::CheckedOut { .. } => write!(f, "the track is checked out"),
            RejectReason::PluginTrack { .. } => write!(f, "tracks with plugins are edited through checkout"),
            RejectReason::PluginOutsideCommit => write!(f, "plugins are added through checkout"),
            RejectReason::NoSuchTrack => write!(f, "the track does not exist any more"),
            RejectReason::NotOwner { .. } => write!(f, "the track is not checked out by you"),
            RejectReason::NotCheckedOut { .. } => write!(f, "the track is not checked out"),
            RejectReason::OutOfScope => write!(f, "the commit reaches outside of its track"),
            RejectReason::MissingBlob(_) => write!(f, "the server did not get all the audio"),
            RejectReason::StemRequired => write!(f, "a track with plugins is committed with its audio"),
            RejectReason::NoSuchEntry => write!(f, "the changelog entry does not exist"),
        }
    }
}

#[derive(Debug)]
pub enum FrameError {
    /// Longer than the limit of its message type, which is the second field.
    TooLong(usize, usize),
    Codec(postcard::Error),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::TooLong(len, max) => write!(f, "frame of {len} bytes exceeds the limit of {max}"),
            FrameError::Codec(e) => write!(f, "malformed frame: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Encodes `msg` as one frame, length prefix included.
pub fn encode<T: Framed>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let frame = postcard::to_extend(msg, vec![0u8; 4]).map_err(FrameError::Codec)?;
    let len = frame.len() - 4;
    if len > T::MAX {
        return Err(FrameError::TooLong(len, T::MAX));
    }
    let mut frame = frame;
    frame[..4].copy_from_slice(&(len as u32).to_le_bytes());
    Ok(frame)
}

/// Length of the payload that follows a frame's four header bytes.
pub fn payload_len<T: Framed>(header: [u8; 4]) -> Result<usize, FrameError> {
    let len = u32::from_le_bytes(header) as usize;
    if len > T::MAX {
        return Err(FrameError::TooLong(len, T::MAX));
    }
    Ok(len)
}

/// Decodes a frame's payload. Trailing bytes are an error.
pub fn decode<T: DeserializeOwned>(payload: &[u8]) -> Result<T, FrameError> {
    let (msg, rest) = postcard::take_from_bytes(payload).map_err(FrameError::Codec)?;
    if !rest.is_empty() {
        return Err(FrameError::Codec(postcard::Error::DeserializeUnexpectedEnd));
    }
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let msg = ClientMsg::Hello(Hello {
            proto_version: 7,
            token: "secret".into(),
            user_key: [9; 16],
            name: "ada".into(),
            resume: Some(Resume {
                prev_actor: ActorId(3),
                session_secret: 42,
                last_seq: 99,
            }),
        });
        let frame = encode(&msg).unwrap();
        let len = payload_len::<ClientMsg>(frame[..4].try_into().unwrap()).unwrap();
        assert_eq!(len, frame.len() - 4);
        assert_eq!(decode::<ClientMsg>(&frame[4..]).unwrap(), msg);
    }

    #[test]
    fn a_snapshot_round_trips() {
        let welcome = ServerMsg::Welcome(Box::new(Welcome {
            actor: ActorId(1),
            user: UserId(1),
            session_secret: 5,
            head_seq: 0,
            sync: SyncData::Snapshot(Project::default()),
            acked: None,
            peers: vec![Peer {
                actor: ActorId(1),
                user: UserId(1),
                name: "ada".into(),
            }],
            transport: TransportState {
                playing: true,
                anchor_tick: 960.5,
                anchor_server_micros: 1_234_567,
                loop_range: Some(mdaw_model::TickRange::new(Tick(0), Tick(3840))),
                epoch: 3,
            },
            history: Vec::new(),
            stale: Vec::new(),
        }));
        let frame = encode(&welcome).unwrap();
        assert_eq!(decode::<ServerMsg>(&frame[4..]).unwrap(), welcome);
    }

    #[test]
    fn bad_frames_are_errors() {
        assert!(matches!(
            payload_len::<ServerMsg>(u32::MAX.to_le_bytes()),
            Err(FrameError::TooLong(..))
        ));
        let between = ((MAX_FRAME + 1) as u32).to_le_bytes();
        assert!(
            payload_len::<ClientMsg>(between).is_err(),
            "too long from a client"
        );
        assert!(payload_len::<ServerMsg>(between).is_ok(), "fine from the server");
        let mut frame = encode(&ClientMsg::Presence(Presence::default())).unwrap();
        frame.push(0);
        assert!(decode::<ClientMsg>(&frame[4..]).is_err());
        assert!(decode::<ServerMsg>(&[0xff, 0xff]).is_err());
    }
}
